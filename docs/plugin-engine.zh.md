# 右侧面板插件引擎分析

> 范围：本文只分析「把右侧工具侧栏做成插件引擎」的可行性与路线，不含代码改动、不引入依赖、不改 Cargo 条目。
> 依据：本仓库当前工作区（分支 `feat/xshell-import`）的真实代码。文末列出证据索引与未验证假设。
> 结论先行，随后给证据。凡本仓库无法核实的外部事实，一律标注「未验证假设」。

---

## 0. 结论先行

1. **右栏现在不是「面板插件体系」，而是一个编译期封闭的单面板**：`ToolSidebar` 用 `ToolKind` 枚举 + 10 个常驻 `Entity` + 2 个懒创建 `Entity`（`files` 按终端创建、`kubernetes` 首次选中创建），渲染时对枚举做 `match`（`src/workspace/tool_sidebar.rs:122-177`、`555-595`）。因此「插件加模块」的本质不是给 dock 多挂一个面板，而是**把封闭枚举换成宿主注册表，并新增一层宿主可解释的 UI 描述**。
2. **插件不能直接产出 GPUI 元素**。面板 trait 的标题签名是 `fn title(&mut self, ...) -> impl IntoElement`，面板名是 `&'static str`（`src/workspace/tool_sidebar.rs:480-553`）；GPUI 的 UI 是 Rust 编译期类型，没有 HTML/DOM，也没有任何 webview 依赖（`Cargo.toml:38-181`，全仓无 wry/webview 的实际使用）。所以插件只能：**返回数据/UI 描述，宿主负责画**；或者由宿主嵌入 webview（本仓库目前没有这项能力）。
3. **推荐路线：分层 UI（L1 控件树 / L2 画布 / L3 交互声明 / L4 webview 兜底）+ 能力 API；首期插件语言用嵌入式 JavaScript（QuickJS 绑定），原生插件用版本化 C ABI 复用同一套协议；不做 V8；WASM 与独立进程留到后续。**
   - 理由：L1 覆盖常规信息面板、表单、表格；L2 用 `gpui-pre 0.3.2` 已公开的 `canvas`、`paint_quad`、`paint_path`、`paint_image`、`with_content_mask` 等原语覆盖自绘；L3 补齐拖拽、上下文菜单、快捷键。插件仍只需要 `host.exec`、主机信息、定时器、存储和通知。
   - 能力边界必须说清：L2 不是任意 GPU 渲染，也没有公开的反向 hit-test API；L4 不在第一版。按第 8 节逐项判断，12 个内置模块里 9 个可由插件完整表达，3 个只能部分表达（文件、Kubernetes、项目），剩余限制来自原生编辑器与宿主专有能力，而不是 UI 表达力。
4. **最容易低估的不是「跑 JS」，而是三件事**：画布层每帧传输/重绘的成本与坐标系一致性、主线程上第三方脚本的可控性（卡死/崩溃/内存）、以及「插件能执行主机命令」带来的信任模型。这三项决定项目最终是「可维护的扩展点」还是「不可控的脚本宿主」。
5. **权限必须先声明、安装时逐项同意、运行时统一强制**：本文定出 17 项权限（从 `ui.render`/`ui.canvas`/`ui.interact` 到 `host.credentials`），所有能力出口经过同一个 `PluginBroker` 闸门；未授予的调用明确失败，不能静默降级。升级新增权限必须重新征得同意；撤销立即停用。原生 DLL 无法被真正强制，只能按完全信任处理（见第 6 节）。
6. **界面分四层、插件不直接画窗口**：L1 定义 20 种节点与 9 类事件，宿主负责布局、主题、虚拟化与错误兜底；L2 提供 8 类绘制操作，坐标用逻辑像素、DPI 由宿主处理；L3 提供拖拽/菜单/快捷键声明；L4 webview 仅作兜底且不在 v1。自绘不再被排除，但原生编辑控件与嵌入终端仍不支持（见第 7 节）。

---

## 1. 右侧面板现状（基于真实代码）

### 1.1 位置与开关

右栏是 GPUI dock 的 Right 区域，里面只有一个 `ToolSidebar` 面板：

- `Workspace` 持有 `tools: Entity<ToolSidebar>`，右侧 dock 注册时 `DockLayout::tabs().panel_view(panel_handle(tools))`（`src/workspace/workspace_view.rs:176-181`、`692-701`）。
- 右栏不是常驻的：只有当前台标签是终端（SSH 或本地）时才可能出现；SFTP 标签、设置页、起始页在前时整个侧栏（含切换按钮）都不显示（`src/workspace/tools.rs:1-16`、`102-135`）。
- 打开/关闭由 `set_right_dock_open` 控制：先临时设为可折叠、toggle、再设回不可折叠，因为「切换按钮是 dock 的唯一控件」（`src/workspace/tools.rs:866-878`）。
- 切换按钮 `render_tool_switch` 画在 dock 区域旁边而不是 dock 内，按钮通过 `target.dispatch_action(&ToggleTool(tool))` 派发到 workspace 的焦点句柄（`src/workspace/tool_sidebar.rs:605-637`）。

### 1.2 模块清单与各自依赖

`ToolKind` 共 12 个（`src/app/actions.rs:622-655`），`ToolSidebar` 为每个工具持有一个面板实体（`src/workspace/tool_sidebar.rs:122-147`）：

| 工具 | `key()` | 面板实体 | 主要依赖 / 数据来源 | 创建时机 |
|---|---|---|---|---|
| 命令片段 | `snippets` | `SnippetPanel` | `HostStore`（片段分类/节点） | 构造时 |
| 历史命令 | `history` | `HistoryPanel` | 终端连接 `exec` | 构造时 |
| 文件 | `files` | `ExplorerPanel`（每个终端一个） | SFTP provider、传输队列 | 首次显示时 |
| tmux | `tmux` | `TmuxPanel` | 终端连接 `exec` | 构造时 |
| Docker | `docker` | `DockerPanel` | 终端连接 `exec` | 构造时 |
| Kubernetes | `kubernetes` | `KubernetesPanel` | 终端连接 `exec` | 首次选中时 |
| 系统服务 | `services` | `ServicePanel` | 终端连接 `exec` | 构造时 |
| 进程管理 | `processes` | `ProcessPanel` | 终端连接 `exec` | 构造时 |
| 网络连接 | `connections` | `NetstatPanel` | 终端连接 `exec` | 构造时 |
| 系统监控 | `monitor` | `MonitorPanel` | 终端连接 `exec` + 定时轮询 | 构造时 |
| GPU/NPU | `gpu` | `GpuPanel` | 终端连接 `exec` | 构造时 |
| 项目 | `projects` | `ProjectPanel` | `HostStore` 的已保存终端、本地终端 | 构造时 |

证据链：

- 字段与 `new()`：`src/workspace/tool_sidebar.rs:122-177`。
- 每个工具「能不能出现在某台主机/某个终端旁」的规则集中在 `impl ToolKind`：`local_only`、`works_on(HostOs)`、`runs_commands`、`LOCAL`（`src/workspace/tool_sidebar.rs:30-65`）。
- 工具按钮 id：`button_id()`（`src/workspace/tool_sidebar.rs:67-83`）。
- 持久化用的 key、默认可见性、文案、图标：`src/app/actions.rs:656-725`。`key()` 的注释明确写了「一旦发布就不能改」，因为它同时写进 `settings.json` 和主机数据库的 `sidebar_tool` 列。

### 1.3 注册与渲染机制

- **注册**：没有动态注册表。`ToolKind::ALL` 是固定长度数组（`src/app/actions.rs:642-655`）；设置页的开关也直接遍历 `ToolKind::ALL`（`src/settings/sidebar_tools.rs:50-54`）。
- **可用性**：`Workspace::offered_tools` 按「当前侧栏目标」过滤：本地终端只给 `LOCAL`；远端则要求 `!local_only && shown && works_on(os) && !(shell_only && runs_commands)`（`src/workspace/tools.rs:143-164`）。
- **切换**：`on_toggle_tool` 校验后写入「该目标记住的工具」，再 `sync_tool_sidebar`（`src/workspace/tools.rs:230-250`）。
- **渲染**：`ToolSidebar::render` 对 `self.tool` 做 `match`，把对应 `Entity` 转成 `AnyElement`；`files` 为 `None` 时画 `Empty`（`src/workspace/tool_sidebar.rs:555-595`）。
- **面板 trait**：`BasePanel::panel_name -> &'static str`、`Panel::title -> impl IntoElement`、`toolbar_buttons`、`zoom_control` 等（`src/workspace/tool_sidebar.rs:480-553`）。这是编译期契约，外部代码无法在运行时实现。
- **设计意图**：注释明确说明「一个面板显示任一工具」而不是「每个工具一个 dock 面板」，原因是换面板会把焦点和面板一起移出 dock，且同一 tab group 里多面板总会画 tab bar（`src/workspace/tool_sidebar.rs:110-121`）。这一点对插件化很关键：**插件做成「dock 里多标签」会直接违背现有交互设计**。

### 1.4 与核心状态的耦合方式

1. **会话 / 终端**
   - 侧栏绑定的目标类型是 `SidebarTarget`：远端为 `ToolTerminal`，本地为 `LocalTerminalId`（`src/workspace/tool_sidebar.rs:86-108`）。
   - `ToolTerminal` 带着：终端 id、主机 id、主机名、探测到的 `HostOs`、`shell_only`、以及 `WeakEntity<TerminalView>`（`src/workspace/tool_sidebar.rs:86-100`）。
   - 工具读主机数据**不开新连接**：统一走 `ExecTarget { terminal, view }`，由 `ToolSidebar::sync_tools` 下发给各面板（`src/workspace/tool_sidebar.rs:390-432`）。
   - 命令执行入口是 `TerminalView::exec(String) -> Option<Receiver<ExecResult>>`（`src/terminal/terminal_view.rs:539-545`）；等待由 `exec_answer` 以 50ms 轮询 `try_recv` 完成（`src/terminal/exec.rs:10-35`）。
   - 取消模型：面板把采样任务放进 `Option<Task<()>>`，`set_target`/重启用赋值覆盖，**丢弃 `Task` 即停止**（`src/monitor/monitor_panel.rs:136-180`；`src/docker/docker_panel.rs:106-119`）。
2. **主机 / 凭据**
   - `HostStore` 是唯一数据源实体，面板通过 `Deref` 只读访问，观察者收到变更通知（`src/host/store.rs:24-60`）。
   - 主机上持久化「上次打开的工具」用数据库列 `sidebar_tool`（`crates/shellrs-hosts/src/model.rs:334-382`；读写逻辑 `src/workspace/tools.rs:279-298`）。
   - 凭据在系统钥匙串，`Host::password_secret()` 只给引用，密码本身不落在 `Host` 结构里（`crates/shellrs-hosts/src/model.rs:449-453`；`src/secrets/keychain.rs:14-33`）。**这是插件权限设计必须尊重的边界。**
3. **SFTP / 传输**
   - 「文件」不是 `ToolSidebar` 自己造的：`Workspace` 为每个远端终端创建一个 `ExplorerPanel`，存进 `file_sidebars`/`explorers`（`src/workspace/workspace_view.rs:187-197`；创建与交接 `src/workspace/tools.rs:811-838`）。
   - `ExplorerPanel` 内部持有 SFTP 命令通道、传输队列、断点续传状态等（`src/explorer/explorer_panel.rs:56-120`）。
   - 结论：**插件想做「文件类」模块，不能只要一个 `exec`，它需要一套宿主代理的 SFTP 能力接口**；这也是首期不建议开放 SFTP 扩展点的原因。
4. **设置 / 持久化**
   - `AppSettings` 是 `serde(default)` 的 JSON，字段缺失即默认值；写入是「临时文件 + 原子替换」（`src/settings/model.rs:21-89`；`src/settings/store.rs:172-181`）。
   - 右栏相关设置：`sidebar_tools`（哪些工具显示）、`local_sidebar_tool`（本地终端上次显示的工具）、`sidebar_widths.right`（`src/settings/model.rs:36-49`）。
   - 数据目录统一在 `src/app/paths.rs:30-81`，`settings.json` 与 `shellrs.db` 并列。
5. **国际化**
   - `rust_i18n::i18n!("locales", fallback = "zh-CN")` 在编译期把 34 个 yml 打进二进制（`src/lib.rs:48-49`；`locales/` 目录）。
   - `t!`/`tn!` 在展示时翻译，语言运行时可切换（`src/i18n.rs:1-13`、`72-109`）。
   - 测试会扫描全部源码，校验 key 存在、占位符一致、没有硬编码中文（`src/i18n.rs:166+`）。**插件文案无法进入这套编译期校验**，必须另行定义规则，否则这条测试会挡住或漏掉插件文案。
6. **主题与图标**
   - 面板统一用 `cx.theme()` 的语义色（`sidebar`、`sidebar_foreground`、`muted_foreground` 等），见 `src/workspace/tool_sidebar.rs:569-570`、`494-520`。
   - 图标来自 gpui-kit 的 `IconName`/`CatalogIcon`（`src/app/actions.rs:706-724`）。插件若允许任意 SVG，需要新的资源加载与安全审查。

### 1.5 已有的抽象点（可以复用，不必重造）

| 抽象点 | 位置 | 插件化后可复用为 |
|---|---|---|
| `ToolKind` 元数据函数（`key/label/icon/works_on/runs_commands/local_only/shown_by_default`） | `src/app/actions.rs:656-725`、`src/workspace/tool_sidebar.rs:30-65` | 插件 manifest 的字段与宿主过滤逻辑 |
| `Panel` / `BasePanel` trait | `src/workspace/tool_sidebar.rs:480-553` | 插件模块在 dock 中的接入方式（但需宿主侧包装） |
| `ExecTarget` + `set_target(target, active)` | `src/workspace/tool_sidebar.rs:203-215`、`390-432` | 插件生命周期里的 `activate/deactivate` 与目标切换 |
| `HostCommand` + `run_on_host` | `src/workspace/tools.rs:881-939` | 插件「执行命令 → 通知 → 刷新」的现成管线 |
| `HostStore` / `SettingsStore` 实体 | `src/host/store.rs:24-60`、`src/settings/store.rs:109-139` | 插件只读状态与配置读取 |
| 侧栏功能开关 UI | `src/settings/sidebar_tools.rs:48-113` | 插件启用/停用界面 |
| CLI 分帧 IPC | `src/cli/protocol.rs:1-31` | 独立进程路线的传输层先例 |

---

## 2. 「往右侧加一个模块」到底要满足什么

把需求拆成 7 项，逐项判定现有能力与插件化后的提供方式。

| 需求 | 现有实现（证据） | 插件化后由谁提供 | 分层 UI 能否满足 |
|---|---|---|---|
| 生命周期 | `new` → `show` → `set_target(target, active)` → `sync_tools`（`tool_sidebar.rs:149-215`、`390-432`、`455-469`） | 宿主调用插件 `onActivate/onDeactivate/onTargetChanged` | 能 |
| 状态访问 | `ToolTerminal{host,name,os,shell_only,view}`、`HostStore`、`SettingsStore`（`tool_sidebar.rs:86-100`） | 宿主只读快照 + 变更事件 | 能（凭据除外） |
| UI 渲染 | `Render` 里的 `match` + `into_any_element`（`tool_sidebar.rs:555-595`） | 插件返回 UI 树（L1）或绘制清单（L2），宿主映射到 gpui 组件/原语 | **能，常规界面用 L1、自绘用 L2** |
| 异步任务 | `exec` + `exec_answer` 轮询；`Task` 丢弃即取消（`terminal/exec.rs:10-35`、`monitor_panel.rs:149-180`） | 宿主提供 `exec(job)`、`setInterval`、`cancel` | 能 |
| 配置持久化 | `settings.json`（`settings/model.rs:21-89`）+ 主机 `sidebar_tool` 列 | 每插件独立配置文件 + 可选按主机分区 | 能 |
| i18n 文案 | 编译期 `t!`/`tn!` + 强校验测试（`src/lib.rs:48-49`、`src/i18n.rs:166+`） | manifest 内建多语言表 + 宿主占位符替换 | 能，但需定规则 |
| 主题与图标 | `cx.theme()` 语义色、`CatalogIcon`/`IconName`（`actions.rs:706-724`） | 宿主给语义色 token；图标限定宿主图标集或受控 SVG | 能 |

### 2.1 哪些必须嵌 webview

分层之后，L2 画布已经覆盖「自绘」这类需求（见 7.8），所以 webview 只留给真正需要浏览器的场景：

- 任意 HTML/CSS/JS 页面、iframe、第三方前端框架（React/Vue）；
- 现成网页登录/OAuth 流程、网页版工具嵌入；
- WebGL/着色器/视频解码等 L2 原语不提供的浏览器能力。

代价：本仓库当前没有任何 webview 集成（`Cargo.toml:38-181` 无 wry 类依赖，全仓检索无实际使用），引入 webview 等于引入新的平台窗口层、GPU 合成与安全面，和 GPUI 的焦点/输入/无障碍体系都要重新对齐。**不建议首期做。**

### 2.2 哪些必须走原生

- 需要直接操作 Rust 侧能力（复用 `shellrs-ssh` 的 SFTP 引擎、终端解析器、端口转发管理器），而不是通过命令字符串或宿主代理；
- 需要原生编辑控件（YAML/代码编辑器、富文本、复杂输入法组合），L1/L2 都无法替代；
- 需要后台线程/自建 socket/系统 API。

这几类是「原生插件」存在的理由，也是它必须承担崩溃风险的原因（见 3.3）。单纯的「自绘」不再是原生插件的理由——L2 可以用宿主绘制原语完成。

---

## 3. 四条宿主路线对比

统一维度：能力上限、实现代价、依赖体积与许可、跨平台（Windows 优先）、崩溃与安全隔离、热更新与调试体验、维护风险。**所有关于第三方 crate 体积/许可/成熟度的数字均为未验证假设**，因为本仓库尚未引入它们，无法从 `Cargo.lock` 核实。

### 3.1 嵌入式 JS 引擎

| 子路线 | 能力上限 | 实现代价 | 依赖体积与许可（未验证） | Windows | 隔离 | 热更新/调试 | 维护风险 |
|---|---|---|---|---|---|---|---|
| QuickJS 绑定（如 rquickjs） | 现代 ES2020 子集；无 DOM/Node API，需宿主补 | 中：需 FFI 封装、异常/内存上限、定时器泵 | 小（C 源码编译，已有 MSVC 工具链，见下）；QuickJS MIT | 好：`cc` 编译，项目已编译 tree-sitter/rusqlite bundled | 进程内，脚本可卡住主线程；需中断/内存上限 | 改文件重载快；调试靠宿主日志/自建断点 | 中：绑定升级、ABI、异常跨边界 |
| Boa（纯 Rust） | 较新的 JS 子集，性能低 | 低（无 C 依赖）→ 高（性能/兼容补齐） | 纯 Rust；MIT（未验证） | 好 | 进程内，同样需预算控制 | 调试工具弱 | 中高：语言实现迭代快、API 变动 |
| V8 / deno_core | 完整 JS + JIT + inspector | 高：构建链、快照、平台线程、FFI 面大 | 很大；MIT（未验证） | 可用但构建重 | 进程内，仍有崩溃风险 | 调试最好（inspector） | 高：升级成本与二进制体积 |

**异步与取消模型（三种都一样要自己设计）**：

- 现有宿主是「请求 → 轮询 receiver」：`exec` 返回 `Receiver<ExecResult>`，`exec_answer` 每 50ms 轮询（`src/terminal/exec.rs:10-35`）。插件侧最自然的映射是回调式：`host.exec(cmd, cb)`，宿主在 gpui 前台把结果回调进引擎。
- 取消：宿主返回 job id，`host.cancel(job)`；同时在引擎侧设置「执行预算/中断」（QuickJS 有中断回调，具体绑定是否暴露为未验证假设），超时直接终止脚本。
- **线程边界成本**：GPUI 的实体与 `Window`/`Context` 都是前台对象（从各面板持有 `Entity`、`FocusHandle`、`Context<Self>` 的用法可以看出，`src/workspace/tool_sidebar.rs:122-177`），不能随意跨线程持有。因此要么把引擎放在 UI 线程（简单，但脚本会阻塞 UI），要么放独立线程（安全，但每次调用都要消息序列化 + 回前台更新）。**这是嵌入式 JS 最大的隐性成本，不是加个 crate 就完事。**

### 3.2 WASM 运行时（wasmtime）

- **能力上限**：任意能编译到 wasm32 的语言（Rust/C/C++/Go/Zig…），沙箱强；但**没有 UI 能力**，仍然要返回 L1 控件树 / L2 绘制清单。
- **实现代价**：高。需要定义 host imports；如果走 component model + WIT，接口更干净，但工具链（wit-bindgen、guest target）与生态成熟度是额外负担；JS 插件要么把 JS 引擎编译成 wasm（如 QuickJS wasm，未验证），要么把 JS 编译成 wasm（Javy 之类，未验证），都和「插件语言不要太复杂」有冲突。
- **依赖体积与许可（未验证）**：wasmtime 依赖 Cranelift，体积与编译时间显著；Apache-2.0 WITH LLVM-exception。
- **Windows**：官方支持良好（未验证具体版本）。
- **隔离**：最好的一档——默认无文件/网络/进程权限，只有宿主给的 import；可用 fuel/epoch 中断实现超时与取消。
- **热更新/调试**：模块可替换；调试需要 wasm 工具链，比 JS 麻烦。
- **维护风险**：中高：WIT 接口一旦发布要长期兼容；component model 仍在演进（成熟度为未验证假设）。
- **结论**：方向正确，但不适合作为首期。等 L1/L2 协议稳定后，WASM 可以作为第三类宿主复用同一协议。

### 3.3 原生动态库

- **根本问题：Rust ABI 不稳定。** 不同 `rustc` 版本间结构体布局、trait object、`Option` 布局、panic 展开都不保证兼容；插件里的 `Entity<T>`、`HostStore` 等类型不能跨 DLL 传递。即使插件也用 Rust 写，也只能在「同一编译器、同一依赖树」下勉强工作，升级宿主即失效。
- **规避方式：C ABI + 版本化结构体**：
  - 插件导出 `extern "C" fn shellrs_plugin_v1(host: *const ShellrsHostApiV1) -> *mut ShellrsPluginV1`；
  - 所有结构体 `#[repr(C)]`，首字段是 `size: u32` 与 `abi_version: u32`，宿主按 size 做兼容读取；
  - 字符串/数组一律用「指针 + 长度」或 JSON 字节，内存由显式 allocator 回调分配/释放，禁止跨边界 `String`/`Vec`；
  - 边界处 `catch_unwind` 或要求 `panic=abort`，禁止 unwind 穿过 C ABI。
- **能力上限**：最高——可以直接用宿主暴露的 C 函数表，也可以自己开线程、读文件；但 UI 仍受限于 L1/L2 协议。
- **隔离**：**最差**。段错误、栈溢出、死循环、内存泄漏都直接带崩整个应用；卸载 DLL 时若有线程/TLS/回调残留会崩。
- **Windows**：`LoadLibrary` 可用（如 `libloading`，ISC，未验证）；但 DLL 搜索路径劫持、代码签名、杀软误报都是现实问题。
- **热更新/调试**：原生调试器好用；热重载只在「无残留线程/无长期存活指针」时可行，而 GPUI 的回调很容易让这条不成立。
- **维护风险**：高。ABI 版本、平台差异、崩溃归因都难。
- **结论**：适合作为**受信任的高级扩展**，不适合作为普通第三方插件的默认形态。

### 3.4 独立进程 + IPC

- **能力上限**：语言无关（任何能读写管道的程序）；UI 仍是 L1/L2 数据；复杂交互（拖拽、逐帧动画）不适合。
- **实现代价**：中高。需要进程生命周期管理、握手/能力协商、订阅/事件流、崩溃重启、Windows 命名管道。
- **既有先例**：`src/cli/protocol.rs:1-31` 已经是「1 字节帧类型 + 4 字节长度 + JSON/原始字节」的分帧协议，Windows 侧也已有 `Win32_System_Pipes` 依赖（`Cargo.toml:165-179`）。这说明传输层不是从零开始。
- **依赖体积与许可**：几乎为零（标准库 + 现有 windows-sys）；进程本身由插件决定。
- **隔离**：最好。插件崩溃只死自己的进程；权限可按进程收紧（不给凭据、只给受限的命令代理）。
- **热更新/调试**：最好。重启进程即热更新；可用任意语言调试器。
- **维护风险**：中：协议版本化、进程孤儿、启动延迟、每帧同步成本。
- **结论**：是「安全第一」时的正确答案，但**UI 与状态同步代价最高**；适合后续做「外部工具型插件」，不适合作为首个版本。

### 3.5 横向对比小结

| 维度 | 嵌入式 JS（QuickJS） | WASM（wasmtime） | 原生 DLL（C ABI） | 独立进程 IPC |
|---|---|---|---|---|
| 能力上限 | 中（逻辑强、UI 受 L1/L2 限制） | 中高（语言多、UI 同受限） | 高（可碰底层） | 中（UI 受协议限制） |
| 实现代价 | 中 | 高 | 中高（ABI 设计） | 中高（协议+进程） |
| 依赖体积/许可 | 较小 / MIT（未验证） | 大 / Apache-2.0（未验证） | 很小 / ISC（未验证） | 很小 / 现有依赖 |
| Windows 优先 | 好 | 好 | 好（但有 DLL 风险） | 好（已有管道代码） |
| 崩溃隔离 | 无 | 强 | 无 | 最强 |
| 安全隔离 | 弱（需自建能力白名单） | 强 | 无 | 强 |
| 热更新 | 好 | 中 | 差 | 最好 |
| 调试 | 中 | 中 | 好 | 最好 |
| 维护风险 | 中 | 中高 | 高 | 中 |

---

## 4. 明确推荐

### 4.1 推荐方案

**宿主主导的「分层 UI + 能力 API」，首期插件运行时为 QuickJS（嵌入式 JS）；原生插件作为第二入口，用 C ABI + 版本化函数表，复用同一套 JSON 消息协议；不采用 V8；WASM 与独立进程作为后续可插拔的第三/第四类宿主。**

分三块（与第 7 节 UI 的四层不是同一维度）：

1. **协议层（先定死）**
   - 插件 → 宿主：`render(ui_tree)`、`canvas.submit(draw_list)`、`canvas.hit(regions)`、`notify(...)`、`setTitle(...)`、`copy(...)`、`log(...)`；
   - 宿主 → 插件：`onActivate(ctx)`、`onDeactivate()`、`onTargetChanged(ctx)`、`onAction(id)`、`onInput(id, value)`、`onTimer(id)`、`onExecDone(job, result)`、`onViewport(...)`、`onHit(...)`、`onDrop(...)`；
   - 所有跨边界数据用 JSON（由 `serde_json` 处理，已有依赖，`Cargo.toml:93`），UI 树与绘制清单都是纯数据。
2. **UI 层（宿主实现）**
   - L1 控件树：20 种节点（见 7.2），覆盖列/行/分栏、文本、按钮、输入、下拉、开关、标签页、列表、表格、树、进度、日志、空态、错误；
   - L2 画布：8 类绘制操作（见 7.8.2），由宿主映射到 `paint_quad`/`paint_path`/`paint_image`/`with_content_mask` 等原语；
   - L3 交互：拖拽、上下文菜单、快捷键声明（见 7.9）；
   - 控件树颜色只用语义 token；画布允许 token+alpha，必要时允许 rgba，但插件需自行负责主题对比度。
   - 图标只开放宿主图标集（`CatalogIcon`）的白名单，不开放任意 SVG 路径（安全与主题一致性）。
3. **运行时层（可替换）**
   - `PluginHost` trait 抽象「加载/调用/取消/卸载」，首期实现 QuickJS；以后加 WASM、原生、进程实现，不改上层。

### 4.2 为什么适合这个项目

- **模块形态与分层匹配**：多数模块是「列表 + 详情 + 按钮 + 定时刷新」，L1 足够；监控柱状条、GPU 条形、K8s 关系图这类自绘由 L2 覆盖，不需要把整个模块推给 webview。
- **命令执行已有稳定管线**：插件要做的核心动作「在主机上跑命令」已经有 `host.exec` 的现成语义（复用终端连接、50ms 轮询、`Task` 取消，`src/terminal/exec.rs:10-35`），不需要插件自己管 SSH。
- **避免引入重型依赖**：项目是单二进制 GPUI 应用（`Cargo.toml:1-58`），V8/deno_core 的体积与构建时间不成比例；QuickJS 的 C 编译与现有 tree-sitter/rusqlite bundled 的构建方式一致。
- **安全边界可以从最小能力开始**：首期插件只给「读主机信息 + 执行只读命令 + 定时器 + 自己的存储」，不给凭据、不给 SFTP、不给任意文件，风险可控。
- **渐进式**：先做「内置模块以 L1 重写一个」验证控件树，再做「监控/GPU 以 L2 重写」验证画布，最后开放给 JS；原生 C ABI 随后；WASM 观察。

### 4.3 能力边界（必须说清，不能夸大）

能做到：

- L1：列表/树/表格/表单/标签页/详情面板；
- L2：矩形、圆角、线段、路径（含圆弧/贝塞尔）、文本、图片、裁剪、变换；足以画柱状图、仪表、关系图、框选矩形、图片预览；
- L3：拖拽源/放置目标、上下文菜单、面板内快捷键；
- 定时刷新、手动刷新、执行命令并解析文本输出；
- 读写插件自己的配置与按主机的键值存储；
- 通知、复制到剪贴板、打开设置页（通过宿主 action）。

做不到（v1 范围内，或不该靠插件做）：

- 原生编辑控件：YAML/代码编辑器、富文本、多行编辑与撤销栈；
- 嵌入终端、终端选择/复制/鼠标上报；
- 外部编辑器、系统文件关联、托盘等系统级集成；
- 无障碍语义树（画布只能给命中区域挂标签）；
- WebGL/着色器/视频、3D；
- 任意 HTML/CSS/JS 页面（只能走 L4 webview，不在 v1）；
- 每帧全量高频刷新（L2 需配合脏区与 30fps 上限，不是无限帧率）；
- 直接调用 `shellrs-ssh`/SFTP 引擎内部 API（必须经 `host.files` 代理，且首期不开放）。

---

## 5. 最小可用版本（MVP）范围建议

### 5.1 首批扩展点

只做一项：**右侧工具侧栏新增一个工具**。不做主机列表右键菜单、不做中心标签页、不做命令行插件。

宿主侧需要新增：

- `PluginRegistry`：扫描 `data_dir/plugins/*/plugin.json`，校验后注册为「插件工具」；
- `ToolKind` 从封闭枚举改为「内置 + 插件」的标识（或新增 `ToolRef`），`key()` 仍是持久化键；
- `ToolSidebar` 增加一个 `Entity<PluginToolPanel>` 用于渲染插件 UI；
- 设置页的侧栏开关复用现有 `SidebarTools` 结构，插件默认可见性由 manifest 决定。

### 5.2 插件清单文件（建议草案）

`<data_dir>/plugins/<id>/plugin.json`：

```json
{
  "id": "com.example.disk-usage",
  "name": { "zh-CN": "磁盘占用", "en": "Disk Usage" },
  "version": "0.1.0",
  "api": 1,
  "engine": "quickjs",
  "entry": "main.js",
  "icon": "hard-drive",
  "targets": ["remote"],
  "requires": { "hostVersion": ">=0.8.0", "os": ["linux", "macos"], "shell": true },
  "permissions": ["ui.render", "host.info", "host.exec", "storage.self", "ui.output"],
  "defaultShown": true
}
```

字段的完整形态（签名、发布者等）见 6.3。

兼容规则：

- 未知字段忽略；未知 `engine`、未知 `permissions`、`api` 不匹配 → 不加载并在设置页提示；
- `icon` 只接受宿主白名单名；
- `name` 必须同时提供 `zh-CN` 与 `en`，否则回退到 `id`。

### 5.3 API 面（建议草案，30 个以内）

| 分组 | 函数 | 说明 |
|---|---|---|
| 生命周期 | `onActivate(ctx)` | 面板可见且目标就绪 |
| | `onDeactivate()` | 面板隐藏或目标切换 |
| | `onTargetChanged(ctx)` | 终端/主机变化 |
| 状态读取 | `host.info()` | `{id,name,os,shellOnly}` |
| | `host.settings()` | 宿主设置只读子集 |
| | `plugin.storage.get(key)` / `set(key, value)` | 插件自己的键值 |
| 命令 | `host.exec(command)` → `jobId` | 在终端自身连接执行 |
| | `host.cancel(jobId)` | 取消 |
| | `onExecDone(jobId, {ok, output, error})` | 回调 |
| 定时 | `timer.setInterval(ms)` → `timerId` | 仅面板激活时计时 |
| | `timer.clear(timerId)` | |
| | `onTimer(timerId)` | |
| UI | `ui.render(tree)` | 提交整棵 UI 树 |
| | `ui.setTitle(text)` | 侧栏标题 |
| | `onAction(id)` / `onInput(id, value)` / `onSelect(id, rowKey)` | 交互回调 |
| 画布 | `ui.canvas.submit(drawList)` | 提交绘制清单（L2） |
| | `ui.canvas.hit(regions)` | 声明命中区域（L2） |
| | `onViewport({width,height,scale,offsetX,offsetY})` | 画布尺寸/滚动变化（L2） |
| | `onFrame(ts)` | 仅在 `animate:true` 时回调（L2） |
| | `onHit({id,x,y,button,modifiers})` | 画布命中（L2） |
| 交互 | `onDragStart(...)` / `onDrop(...)` | 拖拽载荷与投放（L3） |
| | `ui.contextMenu(items)` | 插件面板内的上下文菜单（L3） |
| | `ui.shortcut({id,key,when})` | 面板内快捷键声明（L3） |
| 输出 | `notify(level, text)` | 复用现有 Notification |
| | `clipboard.copy(text)` | |
| | `log(level, text)` | 写入插件日志文件 |
| 国际化 | `t(key, args?)` | 查 manifest 文案表 |
| 菜单 | `ui.setToolbar([...])` | 有限按钮，走宿主 action |

> 总量控制在 30 个以内是刻意的：每多一个函数，都是一份长期兼容承诺。首期不要开放 `host.credentials`、`host.files`、`local.exec`、`network`。

### 5.4 单个模块的示例形态（草案）

以「磁盘占用」为例，插件只做三件事：激活时执行 `df -Pk`、解析输出、渲染表格。

```js
// main.js（草案，非最终 API）
export function onActivate(ctx) {
  ui.setTitle(t("title"));
  refresh();
  timer.setInterval(5000);
}

export function onTimer() { refresh(); }

function refresh() {
  const job = host.exec("df -Pk");
  pending = job;
}

export function onExecDone(job, result) {
  if (job !== pending) return;
  if (!result.ok) {
    ui.render({ type: "empty", text: t("failed", { error: result.error }) });
    return;
  }
  const rows = parseDf(result.output);
  ui.render({
    type: "table",
    columns: [t("mount"), t("size"), t("used"), t("percent")],
    rows: rows.map(r => [r.mount, r.size, r.used, r.percent]),
  });
}

export function onAction(id) {
  if (id === "refresh") refresh();
}
```

宿主侧对应：`PluginToolPanel` 持有 `PluginRuntime`、当前 UI 树、job 映射；`set_target` 时调用 `onActivate/onDeactivate`；`Render` 把 UI 树解释成 gpui 元素。

### 5.5 兼容性承诺（必须预先定死）

- `api: 1` 是**结构版本**，不是插件版本；插件版本用 semver。
- 向后兼容：只增字段、只增 UI 节点类型、只增 capability；删除/改语义必须升 `api`。
- 未知 UI 节点：画占位块并记日志，不让整个面板失败。
- 未知插件能力：拒绝加载，给出明确错误，不静默降级。
- 插件停用/卸载：保留 `plugins/<id>/` 下的数据目录；删除需用户显式确认。
- 宿主升级：插件目录与 `settings.json` 分离，避免设置迁移影响插件。

---

## 6. 权限模型：事先声明、安装同意、运行时强制、撤销

### 6.1 五条硬规则

1. **默认拒绝**：manifest 没声明、用户没授予的能力，运行时一律返回明确错误，不返回空结果、不静默降级。
2. **声明即契约**：权限只写在 manifest 的 `permissions` 里；安装/启用界面逐项展示，用户同意后写入授权记录。
3. **升级不继承新权限**：新版本新增的权限必须重新征得同意；已有权限不能被插件通过配置或运行时调用「自我扩大」。
4. **唯一闸门**：命令、文件、网络、凭据等所有出口都经过同一个宿主代理层检查；不允许任何 API 绕过。
5. **撤销即停用**：撤销权限立即停止插件运行时、取消未完成的任务；插件数据保留但不可访问。

### 6.2 权限清单建议（17 项，按风险分级）

| # | 权限 key | 级别 | 授予后能做什么 | 最坏后果 |
|---|---|---|---|---|
| 1 | `ui.render` | 低 | 在右侧面板内提交声明式 UI 树、接收自己节点的事件 | 伪造类似宿主的界面诱导用户点击；需插件标识与面板边界缓解 |
| 2 | `ui.canvas` | 中 | 提交绘制清单，由宿主用 GPUI 绘制原语逐帧上屏 | 伪造图表/状态误导用户；高频或超量提交拖慢 UI、占用 GPU |
| 3 | `ui.interact` | 中 | 声明拖拽源/接收区、上下文菜单贡献、面板内快捷键 | 误拖触发宿主操作；快捷键抢占；拖拽载荷泄露数据 |
| 4 | `storage.self` | 低 | 读写自己目录下的键值/文件，不能读其他插件 | 明文存敏感数据、占用磁盘 |
| 5 | `schedule.timer` | 低 | 面板激活期间注册定时器 | 高频定时消耗 CPU、拖慢 UI |
| 6 | `ui.output` | 低 | 发系统通知、写剪贴板 | 通知轰炸、覆盖用户剪贴板内容 |
| 7 | `host.info` | 低 | 读当前终端对应主机的非敏感元数据：id、名称、OS、`shell_only` | 把主机名/OS 泄露给插件网络（需叠加 `network`） |
| 8 | `host.list` | 中 | 读全部主机的名称、地址、端口、分组 | 内网资产清单被外传 |
| 9 | `settings.read` | 中 | 读宿主设置的非敏感子集 | 推断用户习惯、本地路径、常用工具 |
| 10 | `host.exec.readonly` | 中高 | 经宿主代理在远端执行**白名单内的只读命令** | 白名单被命令拼接/别名绕过，读取敏感文件 |
| 11 | `host.exec` | 高 | 以当前登录用户在远端执行任意命令 | 完全控制远端主机：读写文件、横向移动、留后门 |
| 12 | `host.files` | 高 | 经宿主 SFTP 代理读、写、删除远端文件 | 窃取、篡改或删除远端数据 |
| 13 | `host.credentials` | 严重 | 读取主机密码/私钥/口令短语 | 凭据泄露，可登录该主机及复用同一凭据的其他主机 |
| 14 | `local.exec` | 高 | 在本机执行命令或启动进程 | 本机任意代码执行、持久化、读取本地所有文件 |
| 15 | `network` | 中高 | 由宿主代理发起出站 HTTP/HTTPS 请求 | 数据外传、C2 回连、内网探测（取决于代理允许的范围） |
| 16 | `webview` | 高 | 在内嵌浏览器中加载页面 | 钓鱼界面、浏览器漏洞面、绕过声明式 UI 与主题/i18n 约束 |
| 17 | `plugin.call` | 高 | 调用其他已启用插件导出的 API | 权限合成提权：低权限插件借高权限插件完成敏感操作 |

说明：

- `host.exec.readonly` 的白名单必须是**命令名 + 参数形状**双重校验，不能只做字符串前缀匹配；参数里出现重定向、管道、命令替换时是否放行需要单独定规则（建议首期拒绝）。
- `host.files` 首期不开放；一旦开放必须配合路径规范化与根目录白名单。
- `host.credentials` 建议**永不开放给 JS 插件**；即使开放，也只应返回「可用于建立连接」的不透明句柄，而不是原始密码/私钥。
- `ui.render` 的风险来自「插件界面伪装成宿主界面」。缓解：插件面板固定显示来源标识，插件不能修改宿主标题栏、工具栏和确认对话框。

### 6.3 清单文件字段形态

在 5.2 的基础上补齐权限、宿主版本与签名：

```json
{
  "id": "com.example.disk-usage",
  "name": { "zh-CN": "磁盘占用", "en": "Disk Usage" },
  "version": "0.2.0",
  "api": 1,
  "engine": "quickjs",
  "entry": "main.js",
  "icon": "hard-drive",
  "assets": ["assets/cpu.png"],
  "targets": ["remote"],
  "requires": {
    "hostVersion": ">=0.8.0 <0.9.0",
    "os": ["linux", "macos"],
    "shell": true
  },
  "permissions": [
    "ui.render",
    "storage.self",
    "schedule.timer",
    "host.info",
    "host.exec.readonly"
  ],
  "publisher": "Example Inc.",
  "homepage": "https://example.com/disk-usage",
  "signature": {
    "algorithm": "ed25519",
    "keyId": "example-2026",
    "value": "base64:..."
  }
}
```

字段约束：

| 字段 | 必填 | 说明 |
|---|---|---|
| `id` | 是 | 反向域名，全局唯一，卸载重装后仍复用其数据目录 |
| `version` | 是 | semver；升级比较依赖它 |
| `api` | 是 | 宿主插件 API 结构版本，首版为 `1` |
| `engine` | 是 | `quickjs`；未来的 `wasm` / `native` 另行定义 |
| `entry` | 是 | 相对于插件目录的入口文件，禁止 `..` 越界 |
| `assets` | 否 | 画布 `image` op 可引用的图片清单；只能是插件目录内的 PNG/JPEG |
| `requires.hostVersion` | 是 | semver 范围；不匹配则拒绝加载 |
| `permissions` | 是 | 只能取 6.2 的 key；未知 key 拒绝加载 |
| `publisher` / `homepage` | 否 | 授权界面展示，便于用户判断来源 |
| `signature` | 视分发策略 | 算法、keyId、签名值；未签名是否允许由开放问题决定 |

### 6.4 授权界面要展示什么

安装/首次启用时，用一个不可跳过的确认页展示：

1. **来源信息**：插件名、版本、作者/发布者、来源路径或仓库、签名状态（已签名/未签名/签名无效）。
2. **权限逐项列表**：每项显示图标、权限名称、风险级别、授予后能做什么、最坏后果；高风险项默认展开说明。
3. **总风险提示**：出现 `host.exec`/`local.exec`/`host.credentials`/`webview` 时给出显式警告，文案不能用「可能」淡化后果。
4. **宿主要求**：最低宿主版本、目标平台、是否仅远端。
5. **操作**：`取消` / `仅本次启用`（不写授权记录，重启失效）/ `同意并启用`。
6. **禁止默认勾选**：用户必须主动确认，不能预置「同意」。

升级时的规则：

- 对比新旧 manifest 的 `permissions`：
  - **新增权限** → 阻止启用，弹同一个逐项授权页，只展示新增项；用户同意后才更新授权记录。
  - **移除权限** → 自动生效，记录日志；不需要用户操作。
  - **清单哈希变化**（入口、权限、签名任一变化）→ 即使权限集合相同也要重新校验签名并提示。
- 授权记录单独存 `data_dir/plugins/grants.json`（不放进 `settings.json`，避免设置迁移和备份互相影响）：

```json
{
  "com.example.disk-usage": {
    "version": "0.2.0",
    "manifestHash": "sha256:...",
    "permissions": ["ui.render", "storage.self", "schedule.timer", "host.info", "host.exec.readonly"],
    "grantedAt": "2026-11-14T10:00:00+08:00"
  }
}
```

- 运行时判断以 `grants.json` 为准，manifest 只是申请；两者不一致（版本或哈希）即视为未授权，必须重新同意。
- 被拒绝的能力调用统一抛出 `PermissionDenied { permission: "host.exec" }`，插件可以捕获并展示自己的错误，但**不能拿到任何替代数据**。

### 6.5 运行时强制点：所有出口过同一道闸

宿主只向插件暴露一个 `PluginBroker`，所有 API 调用先进 `check`，再进入实现：

```rust
// 伪代码：所有 host.* 函数共用
fn call(&self, plugin: &PluginId, api: ApiCall) -> Result<Reply, ApiError> {
    let cap = api.required_capability();          // 每个 API 静态映射权限
    self.gate.check(plugin, cap)?;                // 未授予 => PermissionDenied
    audit::record(plugin, &api);                  // 审计：谁、何时、调了什么
    match api {
        ApiCall::Exec { cmd } => self.exec(plugin, cmd),   // 再叠加只读白名单
        ApiCall::Sftp { op } => self.sftp(plugin, op),     // 路径规范化
        ApiCall::Http { req } => self.http(plugin, req),   // 域名/协议白名单
        ApiCall::Credential { .. } => Err(ApiError::Forbidden), // 首期永不实现
        _ => self.dispatch(api),
    }
}
```

具体出口与检查点：

| 出口 | 检查内容 | 复用点 |
|---|---|---|
| 远端命令 | 权限 + 只读白名单 + 命令形状 | 最终仍走 `TerminalView::exec`（`src/terminal/terminal_view.rs:539-545`），复用现有终端连接与取消模型（`src/terminal/exec.rs:10-35`） |
| 本地命令 | 权限 + 参数白名单（建议首期关闭） | 需要新的本地进程代理，现有代码没有给插件的入口 |
| 远端文件 | 权限 + 路径规范化 + 根目录限制（待定） | 宿主侧 SFTP 代理，参考 `ExplorerPanel` 现有的通道组织（`src/explorer/explorer_panel.rs:56-120`） |
| 网络 | 权限 + 协议/域名白名单 | 新增宿主 HTTP 代理，JS 侧不暴露原始 socket |
| 凭据 | 权限（首期不给）+ 不透明句柄 | `Host::password_secret()` 不直接跨边界（`crates/shellrs-hosts/src/model.rs:449-453`） |
| 插件存储 | 路径必须落在 `data_dir/plugins/<id>/` | `src/app/paths.rs:30-81` |
| 跨插件调用 | 双方都需 `plugin.call`，且被调方显式导出 | 无现成实现 |

**必须诚实说明的强制边界**：

- **JS 插件**（宿主不提供 `fetch`、不提供文件 API）可以被宿主完全强制，因为能力全部由宿主注入。
- **WASM/独立进程**可以由沙箱强制（无 import 即无能力）。
- **原生 DLL 无法真正强制**：一旦加载，插件代码可以直接调用系统 API，绕过 `PluginBroker`。因此原生插件只能按「完全信任」处理：必须签名、安装时单独高强度警告、且不建议作为普通第三方插件的默认形态。

### 6.6 撤销与卸载时的数据处理

- **撤销权限**：立即调用 `onDeactivate` → 取消所有 job/timer → 卸载引擎上下文 → 从 `grants.json` 删除该权限集合。插件目录与数据保留，但插件不能被加载；重新授权后可继续使用原数据。
- **停用（不禁用权限）**：只停止运行时并隐藏面板，保留授权记录。
- **卸载**：确认框写明将删除的插件目录与数据目录路径，并提供「保留数据」选项；默认保留，用户显式勾选才删除。
- **升级导致权限缩小**：立即重启插件运行时，使新权限集合生效。
- **主机删除**：按主机分区的插件数据如何处理尚未定（见开放问题），首期建议保留并在设置页提供「清理孤儿数据」。

---

## 7. 分层 UI 模型：控件树 / 画布 / 交互 / webview

### 7.1 为什么分层，以及各层边界

声明式控件树表达力有限，这是事实；但把所有插件 UI 都推给 webview 也会把主题、输入法、焦点和崩溃隔离一起丢掉。因此改为四层，插件按需组合：

| 层 | 能力 | 权限 | 渲染责任 | 表达上限 | 首版状态 |
|---|---|---|---|---|---|
| L1 控件树 | 信息面板、表单、表格、标签页 | `ui.render` | 宿主渲染，自动继承主题/字体/焦点 | 常规业务界面 | 支持 |
| L2 画布 | 矩形、圆角矩形、线段、路径、文本、图片、裁剪、变换 | `ui.canvas` | 插件给绘制清单，宿主用 GPUI 原语逐帧上屏 | 图表、柱状条、关系图、自定义排版 | 支持（受限） |
| L3 交互声明 | 拖拽源/接收区、上下文菜单、快捷键 | `ui.interact` | 宿主执行拖拽/菜单/快捷键分发 | 与宿主一致的交互语义 | 支持（受限） |
| L4 webview | 运行现成网页 | `webview` | 独立 webview + 隔离 | 任意 HTML/JS | **不在第一版** |

四层不是互斥的：一个插件可以同时用 L1 放表单、L2 画指标、L3 声明拖拽。分层要解决的问题是「哪类表达由谁负责」，而不是再造三套 UI 体系。

共同原则：

1. **宿主拥有基础设施**：布局、主题、字体、焦点、无障碍、输入法、滚动由宿主统一负责；插件不能替换宿主字体栈，也不能修改宿主标题栏、工具栏和确认对话框。
2. **坐标统一用逻辑像素**：L1 的尺寸与 L2 的绘制坐标都以逻辑像素（logical px）为单位，DPI 缩放统一由宿主处理。
3. **提交是数据、不是代码**：无论 L1 的树还是 L2 的绘制清单，都是可校验、可限流的 JSON；宿主不会执行插件给的任何字符串。
4. **只增不改**：节点类型、绘制原语、事件字段只做向后兼容增补；语义变更必须升 `api`。
5. **按层拒绝**：没有 `ui.canvas` 权限时，L2 提交整体失败；没有 `ui.interact` 时，L3 声明被忽略并报错，而不是静默变成不可拖拽。

### 7.2 第一层：控件树（20 种节点）

| # | `type` | 用途 | 关键属性 |
|---|---|---|---|
| 1 | `panel` | 根容器/区域 | `loading`、`padding`、`gap`；根节点唯一 |
| 2 | `section` | 可折叠分组 | `id`、`title`、`collapsed` |
| 3 | `row` | 横向线性布局 | `gap`、`align`、`wrap` |
| 4 | `column` | 纵向线性布局 | `gap`、`align` |
| 5 | `split` | 可拖动分栏 | `direction`、`sizes`、`minSizes` |
| 6 | `scroll` | 滚动容器 | `axis`（`vertical`/`horizontal`） |
| 7 | `text` | 文本 | `value`、`variant`（`body`/`muted`/`heading`/`mono`）、`wrap` |
| 8 | `tag` | 状态标签 | `text`、`tone` |
| 9 | `empty` | 空态 | `text`、`actionId`（可选） |
| 10 | `error` | 错误态 | `message`、`detail`、`retryId` |
| 11 | `button` | 按钮 | `id`、`label`、`tone`（`primary`/`ghost`/`danger`）、`disabled`、`icon` |
| 12 | `input` | 输入框 | `id`、`value`、`placeholder`、`kind`（`text`/`password`/`number`/`search`）、`disabled` |
| 13 | `select` | 下拉选择 | `id`、`value`、`options[{value,label}]` |
| 14 | `toggle` | 开关/复选 | `id`、`checked`、`label` |
| 15 | `tabs` | 标签页 | `id`、`activeId`、`items[{id,label}]` |
| 16 | `list` | 列表 | `items[{id,...}]`、`rowTemplate`、`selection` |
| 17 | `table` | 表格 | `columns[{key,label,width?,align?}]`、`rows[{key,cells}]`、`sortable`、`selection` |
| 18 | `tree` | 树 | `nodes[{id,label,children,expanded,badge}]` |
| 19 | `progress` | 进度/条形指标 | `value`、`max`、`label`、`tone`、`indeterminate` |
| 20 | `log` | 日志流 | `lines[{seq,level,text,time?}]`、`follow`、`maxLines` |

补充约定：

- `divider` 用 `section`/`row` 的 `border` 属性表达，不单列节点。
- `spinner` 用 `panel.loading: true` 或 `empty` 的加载变体表达。
- 详情「键值行」用两列 `table` 表达，不单列节点。
- `list` 的每行允许嵌套 `row`/`text`/`button`/`tag`；`table` 单元格只允许文本、标签、按钮。
- `log` 与 `list`/`table` 由宿主虚拟化，插件不需要自己算可视区。

### 7.3 第一层：布局约束

- **尺寸**：只允许 `auto`、`fill`、`<n>px`、`<n>%`；支持 `minWidth`/`maxWidth`/`minHeight`/`maxHeight`。不支持绝对定位、`top/left`、`z-index`。
- **滚动**：同一轴向只允许一个 `scroll` 容器，禁止嵌套滚动；`list`/`table`/`log` 自带滚动，不能再包 `scroll`。
- **折叠**：`section.collapsed` 与 `tree.nodes[].expanded` 由插件控制，用户点击回传事件。
- **分栏**：`split.sizes` 为比例数组，`minSizes` 防止拖没；用户拖动后宿主回传 `resize` 事件，插件可保存到自己的存储。
- **间距**：统一用 `gap`/`padding` 的数值，建议 4 的倍数；不允许插件指定边距像素细节。
- **宽度自适应**：右栏宽度由宿主控制，插件只能用 `fill` 和百分比，不假设固定像素宽度。
- **字体**：只用宿主字号层级（`heading`/`body`/`muted`/`mono`），不允许指定字号和字族。

### 7.4 第一层：事件回传格式

统一信封（宿主 → 插件）：

```json
{
  "api": 1,
  "type": "event",
  "node": "refresh",
  "event": "click",
  "payload": null
}
```

支持的事件：

| `event` | 触发节点 | `payload` |
|---|---|---|
| `click` | `button`、`empty.actionId`、`error.retryId` | `null` |
| `change` | `input`、`select`、`toggle` | `{ "value": ... }` |
| `select` | `list`、`table` | `{ "row": "rowKey" }` |
| `rowAction` | `list`/`table` 行内按钮 | `{ "row": "rowKey", "action": "nodeId" }` |
| `sort` | `table` | `{ "column": "key", "direction": "asc" }` 或 `"desc"` |
| `tab` | `tabs` | `{ "id": "tabId" }` |
| `toggleSection` | `section` | `{ "collapsed": true }` |
| `resize` | `split` | `{ "sizes": [0.4, 0.6] }` |
| `page` | `table`（分页模式） | `{ "page": 2 }` |

约束：

- 事件里的字符串只是数据；宿主**绝不**把 `payload` 当作命令执行。要执行命令必须由插件显式调用 `host.exec`，并再次过权限闸门。
- `input.change` 由宿主做 200ms 防抖；需要即时反馈时插件自己用 `change` + 本地状态处理，不要依赖高频事件。
- 行内按钮建议 id 统一为 `row:<rowKey>:<action>`，宿主同时回传 `row`，插件不必解析字符串。
- 事件必须在面板激活状态下送达；面板隐藏后宿主丢弃事件并通知插件。

### 7.5 第一层：异步与加载态

- 插件在 `onActivate` / `onTargetChanged` 里同步提交 `panel{loading:true}`，再发起 `host.exec`。
- 每个异步结果带 job id；插件必须忽略过期 job 的结果（参考 5.4 的 `pending` 示例）。
- `onDeactivate` 后宿主取消该插件所有 job 与 timer，迟到的回调不再投递。
- 刷新频率建议不超过 1 次/秒；高频数据由插件聚合后再 `render`。
- 宿主对同一插件的多次 `render` 做合并（只画最后一棵），避免抖动。
- 加载态分三档：首次加载（`panel.loading`）、增量刷新（保留旧数据 + 行内 `progress.indeterminate`）、操作中（按钮 `disabled`）。

### 7.6 第一层：错误态

- **插件业务错误**：用 `error` 节点，包含用户可读的 `message`、可折叠的 `detail`（原始输出）、`retryId`；重试按钮必须能恢复。
- **命令失败 vs 连接断开**：插件应区分并给出不同文案；连接断开时不要提供无意义的重试。
- **宿主渲染错误**：未知节点类型 → 宿主画带节点类型名的占位块并记日志；非法树（缺 id、超限、循环）→ 宿主画统一错误块，面板不空白。
- **权限错误**：`PermissionDenied` 时插件应展示「缺少 X 权限」并提供跳转到设置页的按钮（通过宿主 action），不能伪装成「无数据」。
- **崩溃**：插件运行时异常被宿主捕获后，面板显示错误块 + 「重新加载插件」，不影响其他模块。

### 7.7 第一层：大数据量、分页与虚拟化

- `list`/`table`/`log` 的滚动与虚拟化由宿主实现，插件只提供数据。现有代码已有可复用的 `ListState`/`ListAlignment`（如 `src/services/service_panel.rs:56-57`、`src/docker/docker_panel.rs:59`）。
- 单次提交上限建议：`table` 5,000 行、`list` 5,000 项、`log` 10,000 行；超限宿主截断并在底部显示「已截断」提示。
- 分页两种模式：
  - `page` 模式：`table` 带 `{page, pageSize, total}`，用户翻页回传 `page` 事件；
  - `cursor` 模式：`list` 带 `{nextCursor}`，滚动到底回传 `loadMore`（v1 可只做 page 模式）。
- 排序 v1 由插件处理：宿主只回传 `sort` 事件，插件重新取数/排序后整树重发；宿主不理解业务数据。
- 行 key 必须稳定（如主机 id、进程 pid + 启动时间），否则滚动位置、选中态会错乱。
- `log` 的追加：v1 允许整树重发（宿主 diff），但插件应只保留 `maxLines`；v1.1 再考虑 `log.append` 增量操作。

### 7.8 第二层：画布（插件提交绘制清单，宿主上屏）

视觉原语分 8 类：矩形、圆角矩形、线段、路径、文本、图片、裁剪、变换。下表中矩形与圆角矩形合并为 `rect` 一种 op（`radius` 区分），裁剪与变换用成对的 push/pop op 表达。

#### 7.8.1 已核实的 GPUI 绘制原语

依据 `Cargo.lock:3149-3152` 锁定的 `gpui-pre 0.3.2`，以下 API 是公开可用的（源码位置相对该 crate）：

| 原语 | API | 能力 | 证据 |
|---|---|---|---|
| 画布元素 | `canvas(prepaint, paint)` | 拿到 `Bounds<Pixels>` 与 `&mut Window`，在 paint 回调里绘制 | `src/elements/canvas.rs:10-13` |
| 矩形/圆角/边框 | `Window::paint_quad(PaintQuad)` | 背景（纯色/线性渐变）、四角圆角、四边边框、边框样式 | `src/window.rs:4274`、`7272-7285`；渐变 `src/color.rs:865` |
| 路径 | `Window::paint_path(Path<Pixels>, Background)` | 填充/描边、线宽、虚线、圆弧、贝塞尔、多边形、变换 | `src/window.rs:4345`；`src/path_builder.rs:88-244` |
| 图片 | `Window::paint_image(...)` | 位图、圆角、灰度 | `src/window.rs:4664-4672` |
| 裁剪 | `Window::with_content_mask(...)` | 内容遮罩/裁剪 | `src/window.rs:3726` |
| 图层 | `Window::paint_layer(bounds, f)` | 分组绘制/局部坐标 | `src/window.rs:4123` |
| 文本 | `TextSystem::shape_line(...)` + `ShapedLine::paint(...)` | 用宿主字体栈整形与绘制 | `src/text_system.rs:413-419`、`src/text_system/line.rs:83` |
| 缩放因子 | `Window::scale_factor()` | DPI 换算 | `src/window.rs:2771` |
| 命中盒 | `Window::insert_hitbox(...)` | 给元素插入命中盒 | `src/window.rs:4902` |

本仓库内也有现成用法可参考：`src/explorer/marquee.rs` 用 `canvas` 的 prepaint/paint 画框选矩形，说明这条路径在当前 GPUI 版本上可用。

**待验证 / 未找到的能力**（不要假设它们存在）：

- 没有找到公开的 `Window::hit_test` 或等价的「按坐标反查命中元素」API（现有 `hit_test` 是 crate 内部实现）。因此画布命中测试必须由宿主基于插件声明的区域自行计算。
- 没有找到直接的阴影绘制原语；`BoxShadow` 只存在于样式层（`src/style.rs:349`），画布内阴影只能用多层半透明 quad 或渐变近似。
- `paint_path` 的填充规则（默认 fill rule）、自交路径表现、超长路径性能未验证。
- 文本的富文本 run 能力存在，但画布层 v1 不开放给插件（见 7.8.6）。

#### 7.8.2 绘制清单格式

坐标一律是**画布本地逻辑像素**，宿主在绘制时叠加画布在窗口中的位置与 `scale_factor`。一次提交的信封：

```json
{
  "api": 1,
  "seq": 42,
  "size": { "width": 320, "height": 480 },
  "dirty": [0, 0, 320, 200],
  "ops": [
    { "op": "rect", "x": 8, "y": 8, "w": 120, "h": 6, "radius": 3,
      "fill": { "token": "accent" } },
    { "op": "rect", "x": 8, "y": 8, "w": 120, "h": 6, "radius": 3,
      "stroke": { "width": 1, "color": { "token": "border" } } },
    { "op": "line", "x1": 8, "y1": 20, "x2": 200, "y2": 20,
      "width": 1, "color": { "token": "border" }, "dash": [2, 2] },
    { "op": "path",
      "commands": [["M", 0, 0], ["L", 10, 20], ["Q", 20, 30, 30, 20],
                   ["C", 34, 20, 38, 16, 40, 12], ["A", 4, 4, 0, 0, 1, 48, 12], ["Z"]],
      "fill": { "token": "danger", "alpha": 0.15 },
      "stroke": { "width": 1.5, "color": { "token": "danger" } } },
    { "op": "text", "x": 8, "y": 24, "text": "CPU 42%",
      "variant": "mono", "color": { "token": "foreground" }, "maxWidth": 180 },
    { "op": "image", "x": 0, "y": 0, "w": 16, "h": 16,
      "asset": "cpu", "fit": "contain" },
    { "op": "clip", "rect": [0, 0, 320, 200] },
    { "op": "transform", "translate": [10, 0], "scale": [1, 1], "rotate": 0 },
    { "op": "transformPop" },
    { "op": "clipPop" }
  ]
}
```

操作类型与字段：

| `op` | 字段 | 说明 |
|---|---|---|
| `rect` | `x/y/w/h`、`radius`、`fill`、`stroke{width,color,dash}` | 圆角矩形；`radius` 支持单值或四角数组 |
| `line` | `x1/y1/x2/y2`、`width`、`color`、`dash` | 线段，虚线数组单位为逻辑像素 |
| `path` | `commands`、`fill`、`stroke`、`transform?` | 命令见下；`M/L/Q/C/A/Z` |
| `text` | `x/y`、`text`、`variant`、`color`、`align`、`maxWidth` | 字体由宿主决定，见 7.8.6 |
| `image` | `x/y/w/h`、`asset`、`fit` | `asset` 必须在 manifest 的 `assets` 中登记 |
| `clip` / `clipPop` | `rect`、`radius?` | 裁剪栈，最大深度 8 |
| `transform` / `transformPop` | `translate`、`scale`、`rotate` | 仅影响后续 op，不影响命中区域 |

路径命令：

- `["M", x, y]` 起点；`["L", x, y]` 直线；`["Q", cx, cy, x, y]` 二次贝塞尔；
- `["C", c1x, c1y, c2x, c2y, x, y]` 三次贝塞尔；
- `["A", rx, ry, rotation, largeArc, sweep, x, y]` 圆弧（对应 `PathBuilder::arc_to`，`src/path_builder.rs:155`）；
- `["Z"]` 闭合。

颜色：

- 首选语义 token：`{"token":"accent"}`，可选 `alpha`（0–1）。
- 画布允许 `{"rgba":[r,g,b,a]}`，因为图表需要表达类别色；但宿主会在主题切换时提示「画布未继承主题」，由插件自行负责可读性。
- 不允许渐变对象直接内联到路径以外的复杂结构；线性渐变仅在 `rect.fill` 中支持（对应 `linear_gradient`，`src/color.rs:865`）。

#### 7.8.3 每帧传输与重绘成本控制

- **按需提交**：只有插件调用 `ui.canvas.submit(list)`、画布尺寸变化、主题/缩放变化时才重绘；不跟随 GPUI 的每帧自动重绘。
- **脏区**：`dirty` 为可选的画布本地矩形；宿主与画布边界求交后只重绘该区域。省略即全量。宿主在调试模式下记录实际脏区命中率。
- **提交上限（建议值，需实测）**：单帧 ≤ 5,000 个 op、≤ 1 MB JSON；超限拒绝该帧并回传 `canvasError`，保留上一帧，不空白。
- **帧率**：默认 30 fps 上限；只有提交时带 `"animate": true` 且已授予 `ui.canvas` 才启用 `onFrame(ts)` 回调，取消动画后立即停止回调。
- **静态缓存**：宿主缓存最后一次接受的绘制清单；滚动只改变画布原点时复用清单，不要求插件重传。
- **文本缓存**：宿主按 `(text, variant, maxWidth)` 缓存整形结果，避免每帧重新 shape。
- **图片只传 id**：通过 manifest `assets` 登记，首次加载后宿主缓存 `RenderImage`；禁止每帧 base64。
- **编码**：v1 用 JSON；若实测序列化成为瓶颈，再评估二进制编码（bincode/msgpack），此判断为未验证假设。

#### 7.8.4 命中测试

- 插件在提交绘制清单时，用 `ui.canvas.hit([...])` 声明交互区域；区域坐标与画笔坐标同一空间。
- v1 支持的形状：`rect`、`circle`、`polygon`；路径命中测试延后（避免与填充规则纠缠）。
- 宿主按**从上到下**的顺序做点包含测试，取第一个命中；返回 `{id, x, y, button, modifiers}`。
- `hover` 事件节流到 10 Hz；`click` / `doubleClick` / `contextMenu` 即时投递；拖拽起点必须由 L3 先声明为可拖拽。
- 区域不受 `transform` op 影响：插件必须声明变换后的最终坐标。这样宿主不需要在命中测试里重放变换栈，代价是插件要自己算。
- **未声明区域不穿透**：画布是一个完整的 GPUI 元素，点击落在画布内但不在任何命中区域时，事件不会自动落到底层 L1 节点。插件必须把需要交互的区域声明完整，这是 v1 的硬约束。
- 没有公开的反向 hit-test API，宿主只做「区域 → 点」的正向匹配，不查询 GPUI 内部命中表。

#### 7.8.5 DPI 与缩放

- 插件永远只使用逻辑像素；`window.scale_factor()`（`src/window.rs:2771`）的换算由宿主完成。
- 画布通过 `onViewport` 收到 `{width, height, scale, offsetX, offsetY}`，其中 `width/height` 是逻辑像素视口。
- 描边在宿主侧按 scale 做像素对齐（snap），插件不处理半像素问题。
- 图片由宿主按设备缩放选择采样，插件只提供 1x 资源；是否要求 @2x 资源属于开放问题。
- 显示器 DPI 变化时，宿主保持逻辑尺寸不变、重绘并通知 `onViewport`；插件不需要重建布局。
- **责任划分：DPI 与缩放 100% 在宿主**；插件若按物理像素写死坐标，视为插件错误，宿主可拒绝该帧并报错。

#### 7.8.6 文本如何用宿主字体栈渲染

- 画布的 `text` op 不接收字体族与字号，只接收 `variant`（`body`/`muted`/`heading`/`mono`），由宿主映射到主题字体与字号。
- 宿主用 `TextSystem::shape_line`（`src/text_system.rs:413-419`）整形，`ShapedLine::paint`（`src/text_system/line.rs:83`）上屏，与普通面板使用同一套字体解析。
- 多行文本：宿主按 `maxWidth` 使用 `shape_text` 折行；插件若自己断行，需按行拆成多个 `text` op。
- v1 不支持富文本 run（同一 op 内混排多种字体/颜色/图标）、不支持任意字体、不支持内联图标；图标请用 L1 的 `button.icon` 或 `image` op。

#### 7.8.7 裁剪与滚动如何与第一层配合

- 画布作为 L1 的一个普通元素参与布局，宿主统一应用 `with_content_mask` 裁剪（`src/window.rs:3726`）。
- 两种尺寸模式：
  - `fit: "viewport"`（推荐）：画布高度等于可视区，插件通过 `onViewport` 拿到滚动偏移，只画可见部分，自己做数据虚拟化；
  - `fit: "content"`：画布高度等于内容高度，宿主滚动时重绘整块画布，只适合内容较小或绘制成本低的场景。
- `clip` op 与宿主 content mask 取交集；宿主裁剪优先级更高，插件无法画到面板外。
- 画布不能嵌套画布；滚动容器由 L1 提供，画布不自己接管滚轮。

### 7.9 第三层：交互声明（拖拽 / 上下文菜单 / 快捷键）

#### 7.9.1 拖拽源与放置目标

```json
{
  "drag": { "type": "text/x-shellrs-host-id", "id": "host-row-3" },
  "drop": {
    "accepts": ["text/x-shellrs-host-id", "text/plain"],
    "id": "group-drop"
  }
}
```

- 拖拽数据由插件在 `onDragStart` 中通过 `ui.drag.payload(...)` 提供，宿主校验：
  - 仅允许 JSON 值（字符串/数字/布尔/对象/数组），序列化后 ≤ 64 KB；
  - 不允许二进制、文件句柄、凭据对象；
  - 宿主从不自动把主机密码/私钥放进载荷；插件本身也拿不到凭据（未授予 `host.credentials` 时）。
- 类型字符串使用自定义 MIME 形式 `text/x-shellrs-*`；未知类型宿主拒绝投放。
- 插件只能从自己的面板发起拖拽；v1 不允许插件向宿主主机列表/标签栏注入拖拽源。
- 投放事件回传 `{type, payload, x, y, source}`；目标插件负责校验 payload 语义（例如 id 是否仍存在）。

#### 7.9.2 上下文菜单贡献

- 菜单项是数据：`{id, label, icon?, enabled, danger?, shortcut?}`；点击回传 `{id, target}`。
- 作用域：只能作用于插件自己的面板（L1 节点或 L2 命中区域），不能向宿主主机列表、终端标签页贡献菜单项。
- 权限关系：菜单项本身不携带权限；点击后若插件调用 `host.exec`/`host.files`，仍要过 6.5 的闸门。菜单文案必须如实描述后果（例如「删除容器」不能写成「清理」）。
- 危险项（`danger: true`）由宿主统一加确认样式；插件不能跳过宿主确认对话框。

#### 7.9.3 快捷键绑定

- 插件声明 `{id, key, when: "plugin:<id>", action}`；宿主校验后注册。
- 冲突优先级（建议规则，实现时需按 GPUI keymap 的 predicate 语义验证）：
  1. 文本输入获得焦点时，普通字符与编辑键优先给输入框；
  2. 宿主保留键不可覆盖（如复制/粘贴/关闭标签/全局搜索等，具体清单需定）；
  3. 插件 context 绑定只在插件面板聚焦时生效；
  4. 同一 context 内冲突：宿主内置绑定优先，插件收到冲突报告并被拒绝；
  5. 用户在设置中的自定义覆盖优先于插件默认绑定。
- 现有宿主的绑定已经按 context 限定作用域（`KeyBinding::new(..., Some(HOST_PANEL_CONTEXT))`，`src/app/shortcuts.rs:822-875`），插件沿用同一机制，不新造一套分发器。
- 插件快捷键不能绑定到宿主确认对话框、密码输入等安全上下文。

### 7.10 第四层：webview（仅兜底，不在第一版）

webview 只用于「必须运行现成网页」的场景，不参与 L1/L2 的合成，不自动继承主题与字体。开启前至少需要：

1. 插件必须运行在独立进程或强沙箱中，崩溃不能带崩主程序；
2. `webview` 权限 + 安装时单独高风险确认；
3. URL 白名单与本地资源协议，禁止 `file://` 任意路径与 `nodeIntegration`；
4. CSP、禁 `eval`、禁跨域任意请求，网络仍走宿主代理；
5. 独立 profile/缓存目录，关闭时清理；
6. 与宿主的消息协议只允许调用已声明的能力，不能直接访问宿主内存/状态；
7. 焦点、输入法、剪贴板、快捷键的所有权规则；
8. 签名与发布者校验，未签名插件不得使用 webview。

### 7.11 四层加在一起仍然不支持的表达

- **原生文本编辑控件**：多行编辑器、代码/YAML 编辑器、语法高亮、撤销栈、完整输入法组合。L1 的 `input` 是单行输入，L2 只能画不能编辑。
- **嵌入终端**：终端网格、滚动缓冲、选择与复制、鼠标上报。
- **系统级集成**：外部编辑器打开文件、系统文件关联、托盘/通知中心深度集成、自动更新。
- **无障碍语义树**：画布默认没有语义节点，只能给命中区域挂 `accessibilityLabel`；完整屏幕阅读器支持仍需宿主控件。
- **富文本混排与内联图标**：同一文本 op 内多种字体/颜色/图标。
- **声明式动画/时间线**：L2 可以逐帧提交，但没有 easing、关键帧、过渡等宿主级动画抽象。
- **3D / 自定义着色器**：GPGPU、着色器、视频解码不在画布原语内。
- **任意 HTML/CSS**：只能走 L4。

---

## 8. 现有 12 个右侧模块的分层适配度

判定口径：

- **层的含义**：L1 控件树、L2 画布、L3 交互声明、L4 webview。
- **可完整表达**：现有 UI 与交互都能由这些层组合表达，剩余差异只是「用插件面板替代内置面板」的实现方式。
- **部分表达**：主体可表达，但某一类控件或能力仍必须由宿主提供。
- **最坏情况仍需宿主内实现**：即使四层全开，也不应交给插件（涉及宿主数据写入、原生编辑器、系统集成）。

| 模块 | 需要层 | 判定 | 还差什么 | 代码依据 |
|---|---|---|---|---|
| 命令片段 | L1 + L3 | **可完整表达** | 拖拽入分类与右键菜单由 L3 覆盖；编辑对话框改为面板内 L1 表单；「插入到终端」是宿主 action，不是 UI 短板 | `src/snippets/snippet_panel.rs:409-434`、`663-700` |
| 历史命令 | L1 + L3 | **可完整表达** | 搜索/列表用 L1，右键菜单用 L3；插入终端需宿主 action | `src/history/history_panel.rs:47-105`、`194-254`、`328`/`405` |
| 文件 | L1 + L2 + L3 + `host.files` | **部分表达** | 双栏、拖拽、框选（L2）、图片预览（L2 `paint_image`）都能做；**外部编辑器、系统文件关联、传输调度/断点续传仍是宿主专有**；SFTP 引擎需 `host.files` 代理 | `src/explorer/explorer_panel.rs:56-120`、`src/explorer/marquee.rs` |
| tmux | L1 | **可完整表达** | 表格 + 按钮 + 确认即可 | `src/tmux/tmux_panel.rs:167-285` |
| Docker | L1 | **可完整表达** | 列表 + Tab + `log` 节点覆盖实时输出；详情用 L1 详情面板；无自绘需求 | `src/docker/docker_panel.rs:59`、`298` |
| Kubernetes | L1 + L2 + L3 | **部分表达** | 资源列表、树、关系图（L2）、日志跟随（L1 `log`）可做；**YAML 编辑器是宿主原生编辑控件**，L1/L2 都无法替代 | `src/kubernetes/details.rs:10`、`62-84`、`src/kubernetes/relation_view.rs` |
| 系统服务 | L1 + L3 | **可完整表达** | 表 + 分组 + 详情用 L1，右键菜单用 L3 | `src/services/service_panel.rs:56`、`398`、`448-527` |
| 进程管理 | L1 + L3 | **可完整表达** | 表 + 搜索 + 行内操作 + 详情面板；右键菜单用 L3 | `src/processes/process_panel.rs:254`、`420`、`531` |
| 网络连接 | L1 | **可完整表达** | 过滤器 + 表格 + 行内展开 | `src/netstat/netstat_panel.rs:51-53`、`270`、`518` |
| 系统监控 | L1 + L2 | **可完整表达（画布）** | 每核柱状条、仪表、自适应宽度改用 L2；文字仍由宿主字体栈渲染；布局计算由插件在画布内完成 | `src/monitor/monitor_panel.rs:54-57`、`238-244`、`410-423` |
| GPU/NPU | L1 + L2 | **可完整表达（画布）** | 条形对齐、多指标条形、设备折叠都可以用 L2 + L1 组合；不再受 `progress` 组件限制 | `src/gpu/gpu_panel.rs:660-703`、`703-830` |
| 项目 | L1 + L3 + 宿主写 API | **部分表达** | 树与拖拽（L3）可做；**写主机数据库、启动本地终端、与主机树共享拖拽语义是宿主专有**，且不建议给插件主机写权限 | `src/project/project_panel.rs:294-322`、`517-568`、`src/host/store.rs` |

统计：**可完整表达 9 个**（命令片段、历史命令、tmux、Docker、系统服务、进程管理、网络连接、系统监控、GPU/NPU）；**部分表达 3 个**（文件、Kubernetes、项目）。

**最坏情况仍需宿主内实现的清单**：

1. **Kubernetes 详情页的 YAML 编辑器**：需要语法高亮、撤销栈、输入法组合与搜索，属于宿主编辑控件，不应由插件重造。
2. **文件模块的外部编辑器与传输调度**：打开系统编辑器、文件关联、断点续传、传输队列属于宿主专有；插件最多通过 `host.files` 做受限操作。
3. **项目模块的主机数据写入与本地终端启动**：涉及 `HostStore` 写操作与本地 shell 生命周期，不应开放给插件。
4. **终端本身**：无论哪一层都不应让插件嵌入或接管终端。

结论修正：

- 上一版受限的模块里，命令片段、历史命令、Docker、系统服务、进程管理因 L3 补齐；系统监控、GPU/NPU 因 L2 补齐；文件与项目仍为部分表达，Kubernetes 仍受限于 YAML 编辑器。**画布层确实解除了「自绘」这一主要限制**。
- 真正剩余的短板不再是「UI 表达力」，而是**原生编辑控件与宿主专有能力**；这两类不该用插件重造。
- 因此 v1 可以承诺「现有 12 个模块中 9 个可由插件完整表达」，但不能承诺「全部」，也不能承诺「插件可替代内置实现」；内置模块继续由宿主维护。

---

## 9. 实现前必须由用户拍板的开放问题

1. **凭据访问**：插件能否读取主机用户名/密码/私钥？建议首期完全禁止，只给 `host.info()` 的非敏感字段（`id/name/os/shellOnly`）。
2. **命令执行**：插件能否执行任意命令？建议首期只允许通过宿主代理执行，并对「写操作」要求显式声明（类似 Docker 的确认对话框），且默认禁止 `sudo`/交互式命令。
3. **分发与信任模型**：插件从哪来（本地目录 / 官方市场 / Git 仓库）？是否要求签名？未签名插件是否默认禁用？现有更新机制已有 minisign（`Cargo.toml:120`），可复用其信任思路，但**插件签名是另一套密钥与吊销流程**。
4. **是否允许插件自带 webview**：这会推翻「宿主渲染」的前提，并引入新的安全面。建议首期明确禁止；若未来开放，必须满足 7.10 的全部前置条件。
5. **插件文案与 i18n**：允许 manifest 自带文案（则现有强校验测试要放行插件目录），还是强制插件只能使用宿主提供的 key（表达力受限）。
6. **崩溃与遥测责任**：插件导致卡死/崩溃时，是否上报、是否自动禁用、是否影响「匿名使用统计」（`src/analytics/`）。
7. **按主机隔离的数据**：插件存储是否需要按 `hostId` 分区？主机删除后插件数据如何处理？
8. **许可证问题**：宿主是 GPL-3.0-only（`Cargo.toml:7`）。把第三方插件在进程内加载，是否构成衍生作品、插件是否必须 GPL 兼容——**这是法律问题，本文无法判断，标为未验证**。若走独立进程路线，这个问题会小很多。
9. **性能预算**：单个插件每次 `exec`/渲染的耗时上限、内存上限、并发插件数量上限。
10. **交互冲突**：插件按钮能否向终端输入命令（类似「命令片段」的 `EnterCommand`，`src/workspace/tools.rs:364-410`）？还是只读展示？

---

## 10. 未验证假设

以下内容本仓库无法核实，实施前需要单独验证：

1. QuickJS 绑定（如 rquickjs）的具体许可、维护活跃度、Windows MSVC 构建体验、是否暴露中断与内存上限 API。
2. Boa 的 JS 兼容度、性能与 API 稳定性。
3. deno_core/V8 的二进制体积增量、构建时间、Windows 静态链接可行性。
4. wasmtime 的 component model + WIT 在 Rust guest/host 两侧的成熟度、体积增量、Windows 支持细节。
5. 各候选依赖的许可证文本与 GPL-3.0-only 的兼容性结论（尤其 V8/deno_core 相关依赖树）。
6. L1 控件树在 GPUI 上的实际性能（大树 diff、滚动列表虚拟化）尚未做原型。
7. 插件在主线程执行时的卡顿阈值，以及把引擎放独立线程后的序列化开销，均未实测。
8. 「12 个内置模块中有多少能完整表达」是按代码结构估计，未逐模块做原型；文件和 K8s 的结论最可能随实现变化。
9. GPUI 对嵌入原生窗口/webview 的支持程度未在本仓库验证（当前没有任何相关代码）。
10. 插件签名/分发流程与现有更新机制（`src/update/`）的复用边界未评估。
11. `paint_path` 的默认填充规则、自交路径表现、超长路径性能未验证；圆弧 `arc_to` 的边界行为需原型确认。
12. 画布命中测试只能由宿主基于插件声明区域做正向匹配（未找到公开的反向 hit-test API）；该结论基于 `gpui-pre 0.3.2` 源码检索，若后续版本提供公开 API 可简化设计。
13. 画布每帧 JSON 序列化 + `Vec<op>` 校验 + GPUI 重绘的实际成本未实测；5,000 op / 1 MB / 30 fps 均为建议值。
14. 画布文本与输入法的交互（画布不可编辑，但滚动/焦点切换时 IME 状态如何收口）未验证。
15. 画布内图片按设备缩放采样的画质与显存占用未实测。

---

## 11. 证据索引

| 主题 | 文件与位置 |
|---|---|
| 工具枚举、key、文案、图标、默认可见 | `src/app/actions.rs:622-725` |
| 工具适用性（本地/远端/OS/shell_only） | `src/workspace/tool_sidebar.rs:30-84` |
| `ToolSidebar` 字段、构造、懒创建 | `src/workspace/tool_sidebar.rs:122-177` |
| 目标同步、`show`、`Render` | `src/workspace/tool_sidebar.rs:203-215`、`390-432`、`455-469`、`555-595` |
| `Panel`/`BasePanel` 契约 | `src/workspace/tool_sidebar.rs:480-553` |
| 切换按钮 | `src/workspace/tool_sidebar.rs:605-637` |
| 右栏打开/关闭 | `src/workspace/tools.rs:200-250`、`787-805`、`866-878` |
| 可用工具过滤 | `src/workspace/tools.rs:143-164` |
| 主机侧记录的工具 | `src/workspace/tools.rs:279-298` |
| 命令执行管线 | `src/workspace/tools.rs:881-939` |
| 终端 `exec` | `src/terminal/terminal_view.rs:539-545`、`src/terminal/exec.rs:10-35` |
| 轮询/取消模型 | `src/monitor/monitor_panel.rs:34-64`、`136-180`；`src/docker/docker_panel.rs:106-119` |
| 右 dock 注册 | `src/workspace/workspace_view.rs:692-701` |
| `Workspace` 状态字段 | `src/workspace/workspace_view.rs:169-272` |
| 主机存储实体 | `src/host/store.rs:24-60` |
| 主机模型与 `sidebar_tool` | `crates/shellrs-hosts/src/model.rs:334-382` |
| 密码引用 | `crates/shellrs-hosts/src/model.rs:449-453`；`src/secrets/keychain.rs:14-33` |
| SFTP/传输面板 | `src/explorer/explorer_panel.rs:56-120` |
| 设置模型与写入 | `src/settings/model.rs:21-89`；`src/settings/store.rs:172-181` |
| 侧栏功能设置 UI | `src/settings/sidebar_tools.rs:48-113` |
| 数据目录 | `src/app/paths.rs:30-81` |
| i18n 编译期加载与校验 | `src/lib.rs:48-49`；`src/i18n.rs:1-13`、`72-109`、`166+` |
| CLI 分帧 IPC | `src/cli/protocol.rs:1-31` |
| 依赖与许可证 | `Cargo.toml:1-58`（GPL-3.0-only 在第 7 行）、`38-181` |
| GPUI 版本锁定 | `Cargo.lock:3149-3152`（`gpui-pre 0.3.2`） |
| 画布与绘制原语 | `gpui-pre-0.3.2/src/elements/canvas.rs:10-13`；`src/window.rs:2771`（scale_factor）、`3726`（content mask）、`4123`（paint_layer）、`4274`（paint_quad）、`4345`（paint_path）、`4664-4672`（paint_image）、`4902`（insert_hitbox）、`7272-7285`（PaintQuad 字段）；`src/path_builder.rs:88-244`；`src/text_system.rs:413-419`；`src/text_system/line.rs:83` |
| 本仓库画布用法 | `src/explorer/marquee.rs`（`canvas` + prepaint/paint 手绘） |
| 快捷键 context | `src/app/shortcuts.rs:822-875` |
| 模块拖拽/菜单证据 | `src/snippets/snippet_panel.rs:409-434`、`663-700`；`src/project/project_panel.rs:294-322`、`517-568`；`src/history/history_panel.rs:328`、`405`；`src/services/service_panel.rs:398`；`src/processes/process_panel.rs:420`、`531` |

---

## 12. 一页纸结论

- 能做，但不是「加个插件目录」：需要一个**分层 UI 解释器** + **能力 API** + **运行时抽象**。
- 权限先声明、安装逐项同意、运行时统一闸门：17 项权限、`PluginBroker` 单一检查点、升级新增权限重新同意、撤销立即停用；原生 DLL 只能按完全信任处理。
- 界面分四层：L1 共 20 种节点 + 9 类事件；L2 共 8 类绘制操作，基于 `gpui-pre 0.3.2` 的 `canvas`/`paint_quad`/`paint_path`/`paint_image`/`with_content_mask`；L3 拖拽/菜单/快捷键；L4 webview 不在 v1。宿主负责布局、主题、DPI、虚拟化与错误兜底。
- 现有 12 个模块：9 个可完整表达（含监控、GPU 借画布补齐），3 个只能部分表达（文件、Kubernetes、项目）；剩余限制来自原生编辑器与宿主专有能力，不是 UI 表达力。
- 最坏情况仍需宿主内实现：K8s 的 YAML 编辑器、文件的外部编辑器/传输调度、项目的主机写入与本地终端、终端本身。
- 首期建议只支持 JS（QuickJS）+ 只读主机信息 + 受控 `exec` + 定时器 + 插件私有存储；原生用 C ABI 后置；WASM/进程后置。
- 不建议把插件做成「dock 多标签」，会破坏现有单面板/焦点设计（`src/workspace/tool_sidebar.rs:110-121`）。
- 三个决定成败的风险：画布每帧传输/重绘成本与坐标系一致性、主线程脚本的可控性、插件执行主机命令的信任模型。
