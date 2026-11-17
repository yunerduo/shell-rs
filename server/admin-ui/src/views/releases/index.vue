<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  getPackageSpecs,
  getReleaseHashes,
  getReleaseOverview,
  publishRelease,
  republishRelease,
  uploadPackage,
  withdrawRelease,
  type PackageSpec,
  type ReleaseHash,
  type ReleaseOverview,
  type ReleaseRecord
} from "@/api/admin";

defineOptions({ name: "ReleaseManage" });

const loading = ref(false);
const drawer = ref(false);
const publishing = ref(false);
const specsLoading = ref(false);
const overview = ref<ReleaseOverview>({
  stable: null,
  beta: null,
  manifests: [],
  releases: [],
  records: []
});
const specs = ref<PackageSpec[]>([]);
const picks = reactive<Record<string, File>>({});
const progress = reactive<Record<string, number>>({});
const hashDialog = ref(false);
const hashLoading = ref(false);
const hashVersion = ref("");
const hashRows = ref<ReleaseHash[]>([]);

const form = reactive({
  channel: "stable",
  version: "",
  severity: "recommended",
  minimumVersion: "",
  rollout: 1,
  notes: "",
  notesUrl: ""
});

const pickedFiles = computed(() => Object.values(picks));

function severityLabel(severity: string) {
  if (severity === "required") return "强制更新";
  if (severity === "recommended") return "建议更新";
  return severity;
}

function severityType(severity: string) {
  return severity === "required" ? "warning" : "success";
}

// 强制更新默认全量，否则只有抽中灰度的机器会被阻断。
watch(
  () => form.severity,
  severity => {
    if (severity === "required") form.rollout = 1;
  }
);

function formatBytes(bytes: number) {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function shortDate(at: string) {
  return at ? at.slice(0, 10) : "";
}

function preview(text: string) {
  if (!text) return "这个版本没有写更新说明。";
  return text.length > 140 ? `${text.slice(0, 140)}…` : text;
}

async function loadOverview() {
  loading.value = true;
  try {
    const result = await getReleaseOverview();
    overview.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "读取发布信息失败");
  } finally {
    loading.value = false;
  }
}

async function loadSpecs() {
  const version = form.version.trim();
  if (!version || !/^[A-Za-z0-9.+-]+$/.test(version)) {
    specs.value = [];
    return;
  }
  specsLoading.value = true;
  try {
    const result = await getPackageSpecs(version);
    specs.value = result.data;
  } catch (error: any) {
    specs.value = [];
    ElMessage.error(error?.response?.data?.message || "读取安装包清单失败");
  } finally {
    specsLoading.value = false;
  }
}

function openDrawer() {
  form.channel = "stable";
  form.version = "";
  form.severity = "recommended";
  form.minimumVersion = "";
  form.rollout = 1;
  form.notes = "";
  form.notesUrl = "";
  specs.value = [];
  Object.keys(picks).forEach(key => delete picks[key]);
  Object.keys(progress).forEach(key => delete progress[key]);
  drawer.value = true;
}

function pick(fileKey: string, raw: File | undefined) {
  if (!raw) return;
  picks[fileKey] = raw;
  progress[fileKey] = 0;
}

async function showHashes(version: string) {
  hashVersion.value = version;
  hashRows.value = [];
  hashDialog.value = true;
  hashLoading.value = true;
  try {
    const result = await getReleaseHashes(version);
    hashRows.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "计算摘要失败");
  } finally {
    hashLoading.value = false;
  }
}

async function withdraw(record: ReleaseRecord) {
  try {
    await ElMessageBox.confirm(
      `撤下 ${record.channel} 通道的 v${record.version}？撤下后更新清单不再下发，安装包也停止下载；发布记录保留，可以补发。`,
      "撤下版本",
      { type: "warning", confirmButtonText: "撤下", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await withdrawRelease(record.version, record.channel);
    ElMessage.success(`v${record.version} 已从 ${record.channel} 撤下`);
    await loadOverview();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "撤下失败");
  }
}

async function republish(record: ReleaseRecord) {
  try {
    await ElMessageBox.confirm(
      `补发 v${record.version} 到 ${record.channel}？会重新生成并签名清单，发布时间更新为现在。`,
      "补发版本",
      { type: "info", confirmButtonText: "补发", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await republishRelease(record.version, record.channel);
    ElMessage.success(`v${record.version} 已补发到 ${record.channel}`);
    await loadOverview();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "补发失败");
  }
}

async function submit() {
  const version = form.version.trim();
  if (!/^[A-Za-z0-9.+-]+$/.test(version)) {
    ElMessage.warning("版本号只能用字母、数字、点、加号和短横线");
    return;
  }
  if (!specs.value.length) {
    await loadSpecs();
  }
  if (!specs.value.length) {
    ElMessage.warning("先填版本号，确认安装包清单");
    return;
  }
  if (!pickedFiles.value.length) {
    ElMessage.warning("至少选一个安装包");
    return;
  }
  publishing.value = true;
  try {
    for (const spec of specs.value) {
      const file = picks[spec.file];
      if (!file) continue;
      progress[spec.file] = 1;
      await uploadPackage(version, spec.file, file, percent => {
        progress[spec.file] = percent;
      });
      progress[spec.file] = 100;
    }
    await publishRelease({
      channel: form.channel,
      version,
      severity: form.severity,
      notes: form.notes,
      notesUrl: form.notesUrl,
      minimumVersion: form.minimumVersion,
      rollout: Number(form.rollout)
    });
    ElMessage.success(`${version} 已发布到 ${form.channel}`);
    drawer.value = false;
    await loadOverview();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || error?.message || "发布失败");
  } finally {
    publishing.value = false;
  }
}

onMounted(loadOverview);
</script>

<template>
  <div class="release-page">
    <div class="page-head">
      <div>
        <h2>版本发布</h2>
        <p class="muted">
          上传安装包、填版本与更新说明，发布后清单立即在公网生效。清单由服务器签名。
        </p>
      </div>
      <el-button v-perms="'release:publish'" type="primary" @click="openDrawer">
        发布新版本
      </el-button>
    </div>

    <el-row :gutter="16" v-loading="loading">
      <el-col :xs="24" :md="12">
        <el-card shadow="never" class="channel-card">
          <template #header>
            <div class="channel-head">
              <span>stable 稳定通道</span>
              <el-tag v-if="overview.stable" :type="severityType(overview.stable.severity)" size="small">
                {{ severityLabel(overview.stable.severity) }}
              </el-tag>
            </div>
          </template>
          <template v-if="overview.stable">
            <h3>v{{ overview.stable.version }}</h3>
            <p class="muted small">
              {{ shortDate(overview.stable.publishedAt) }}
              <a v-if="overview.stable.notesUrl" :href="overview.stable.notesUrl" target="_blank">
                更新说明
              </a>
            </p>
            <p class="notes">{{ preview(overview.stable.notes) }}</p>
          </template>
          <p v-else class="muted">还没有发布。</p>
        </el-card>
      </el-col>
      <el-col :xs="24" :md="12">
        <el-card shadow="never" class="channel-card">
          <template #header>
            <div class="channel-head">
              <span>beta 测试通道</span>
              <el-tag v-if="overview.beta" :type="severityType(overview.beta.severity)" size="small">
                {{ severityLabel(overview.beta.severity) }}
              </el-tag>
            </div>
          </template>
          <template v-if="overview.beta">
            <h3>v{{ overview.beta.version }}</h3>
            <p class="muted small">
              {{ shortDate(overview.beta.publishedAt) }}
              <a v-if="overview.beta.notesUrl" :href="overview.beta.notesUrl" target="_blank">
                更新说明
              </a>
            </p>
            <p class="notes">{{ preview(overview.beta.notes) }}</p>
          </template>
          <p v-else class="muted">还没有发布。</p>
        </el-card>
      </el-col>
    </el-row>

    <el-card shadow="never" class="block">
      <template #header>已发布的清单</template>
      <el-table :data="overview.manifests" size="small">
        <el-table-column prop="channel" label="通道" width="110">
          <template #default="{ row }">
            <el-tag :type="row.channel === 'beta' ? 'info' : 'primary'" size="small">
              {{ row.channel }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="version" label="版本" min-width="140">
          <template #default="{ row }">v{{ row.version }}</template>
        </el-table-column>
        <el-table-column label="更新分级" width="120">
          <template #default="{ row }">
            <el-tag :type="severityType(row.severity)" size="small">{{ severityLabel(row.severity) }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="发布时间" width="140">
          <template #default="{ row }">{{ shortDate(row.publishedAt) }}</template>
        </el-table-column>
        <el-table-column label="状态" width="110">
          <template #default="{ row }">
            <el-tag v-if="row.withdrawn" type="info" size="small">已撤下</el-tag>
            <el-tag v-else type="success" size="small">下发中</el-tag>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-card shadow="never" class="block">
      <template #header>安装包</template>
      <el-table :data="overview.releases" size="small">
        <el-table-column type="expand">
          <template #default="{ row }">
            <el-table :data="row.files" size="small" class="file-table">
              <el-table-column label="文件" min-width="280">
                <template #default="{ row: file }">
                  <a :href="`/releases/${row.version}/${file.name}`" target="_blank">
                    <code>{{ file.name }}</code>
                  </a>
                </template>
              </el-table-column>
              <el-table-column label="平台" width="130">
                <template #default="{ row: file }">{{ file.platform }}</template>
              </el-table-column>
              <el-table-column label="架构" width="100">
                <template #default="{ row: file }">{{ file.arch }}</template>
              </el-table-column>
              <el-table-column label="大小" width="100">
                <template #default="{ row: file }">{{ formatBytes(file.size) }}</template>
              </el-table-column>
              <el-table-column label="SHA-256" min-width="240">
                <template #default="{ row: file }"><code class="hash">{{ file.sha256 }}</code></template>
              </el-table-column>
              <el-table-column label="下载" width="90">
                <template #default="{ row: file }">{{ file.downloads }}</template>
              </el-table-column>
              <el-table-column label="上传时间" width="120">
                <template #default="{ row: file }">{{ shortDate(file.uploadedAt) }}</template>
              </el-table-column>
              <el-table-column label="状态" width="100">
                <template #default="{ row: file }">
                  <el-tag v-if="file.withdrawn" type="info" size="small">已撤下</el-tag>
                  <el-tag v-else type="success" size="small">可下载</el-tag>
                </template>
              </el-table-column>
            </el-table>
          </template>
        </el-table-column>
        <el-table-column prop="version" label="版本" min-width="140">
          <template #default="{ row }">v{{ row.version }}</template>
        </el-table-column>
        <el-table-column label="文件数" width="100">
          <template #default="{ row }">{{ row.files.length }}</template>
        </el-table-column>
        <el-table-column label="体积" width="120">
          <template #default="{ row }">{{ formatBytes(row.totalSize) }}</template>
        </el-table-column>
        <el-table-column label="下载次数" width="110">
          <template #default="{ row }">{{ row.downloads }}</template>
        </el-table-column>
        <el-table-column label="操作" width="120">
          <template #default="{ row }">
            <el-button link type="primary" @click="showHashes(row.version)">
              校验摘要
            </el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-card shadow="never" class="block">
      <template #header>
        <div class="channel-head">
          <span>升级记录</span>
          <span class="muted small">每次发布、补发、撤下都在这里留痕；撤下后可以补发。</span>
        </div>
      </template>
      <el-table :data="overview.records" size="small">
        <el-table-column prop="version" label="版本" min-width="130">
          <template #default="{ row }">v{{ row.version }}</template>
        </el-table-column>
        <el-table-column prop="channel" label="通道" width="100">
          <template #default="{ row }">
            <el-tag :type="row.channel === 'beta' ? 'info' : 'primary'" size="small">
              {{ row.channel }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="更新分级" width="110">
          <template #default="{ row }">
            <el-tag :type="severityType(row.severity)" size="small">{{ severityLabel(row.severity) }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="包数" width="80">
          <template #default="{ row }">{{ row.packageCount }}</template>
        </el-table-column>
        <el-table-column label="体积" width="110">
          <template #default="{ row }">{{ formatBytes(row.totalSize) }}</template>
        </el-table-column>
        <el-table-column label="下载次数" width="110">
          <template #default="{ row }">{{ row.downloads }}</template>
        </el-table-column>
        <el-table-column label="发布时间" width="140">
          <template #default="{ row }">{{ shortDate(row.publishedAt) }}</template>
        </el-table-column>
        <el-table-column label="状态" width="100">
          <template #default="{ row }">
            <el-tag v-if="row.withdrawn" type="info" size="small">已撤下</el-tag>
            <el-tag v-else type="success" size="small">已发布</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="150" fixed="right">
          <template #default="{ row }">
            <el-button
              v-if="!row.withdrawn"
              v-perms="'release:publish'"
              link
              type="danger"
              @click="withdraw(row as ReleaseRecord)"
            >
              撤下
            </el-button>
            <el-button
              v-else
              v-perms="'release:publish'"
              link
              type="primary"
              @click="republish(row as ReleaseRecord)"
            >
              补发
            </el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog v-model="hashDialog" :title="`v${hashVersion} 安装包摘要`" width="860px">
      <el-table v-loading="hashLoading" :data="hashRows" size="small">
        <el-table-column prop="name" label="文件" min-width="300">
          <template #default="{ row }"><code>{{ row.name }}</code></template>
        </el-table-column>
        <el-table-column label="平台" width="130">
          <template #default="{ row }">{{ row.platform }}</template>
        </el-table-column>
        <el-table-column label="架构" width="100">
          <template #default="{ row }">{{ row.arch }}</template>
        </el-table-column>
        <el-table-column label="体积" width="110">
          <template #default="{ row }">{{ formatBytes(row.size) }}</template>
        </el-table-column>
        <el-table-column label="SHA-256" min-width="320">
          <template #default="{ row }"><code class="hash">{{ row.sha256 }}</code></template>
        </el-table-column>
      </el-table>
      <p class="muted small">
        摘要按磁盘上的文件实时计算。客户端下载后会按清单里的同一份 SHA-256 校验。
      </p>
    </el-dialog>

    <el-drawer v-model="drawer" title="发布新版本" size="760px" destroy-on-close>
      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :span="12">
            <el-form-item label="通道">
              <el-select v-model="form.channel" class="w-full">
                <el-option label="stable（稳定通道）" value="stable" />
                <el-option label="beta（测试通道）" value="beta" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="版本号">
              <el-input
                v-model="form.version"
                placeholder="0.1.20"
                @blur="loadSpecs"
              />
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="更新分级">
              <el-select v-model="form.severity" class="w-full">
                <el-option label="建议更新" value="recommended" />
                <el-option label="强制更新" value="required" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="最低可自动升级版本">
              <el-input v-model="form.minimumVersion" placeholder="留空表示不限" />
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="灰度比例（0–1）">
              <el-input-number v-model="form.rollout" :min="0" :max="1" :step="0.05" class="w-full" />
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="说明链接">
              <el-input v-model="form.notesUrl" placeholder="留空自动指向 /changelog#v版本号" />
            </el-form-item>
          </el-col>
        </el-row>

        <el-form-item label="更新说明（Markdown）">
          <el-input
            v-model="form.notes"
            type="textarea"
            :rows="8"
            placeholder="### 新增&#10;&#10;- ..."
          />
        </el-form-item>

        <el-form-item label="安装包">
          <div class="specs">
            <p v-if="specsLoading" class="muted">正在读取这个版本的安装包清单…</p>
            <p v-else-if="!specs.length" class="muted">
              先填版本号并离开输入框，下面会列出这个版本需要的文件名。
            </p>
            <el-table v-else :data="specs" size="small">
              <el-table-column prop="platform" label="平台" width="190" />
              <el-table-column label="文件名" min-width="300">
                <template #default="{ row }">
                  <code>{{ row.file }}</code>
                </template>
              </el-table-column>
              <el-table-column label="选择" width="240">
                <template #default="{ row }">
                  <el-upload
                    :auto-upload="false"
                    :show-file-list="false"
                    :on-change="file => pick(row.file, file.raw)"
                  >
                    <el-button size="small">选择文件</el-button>
                  </el-upload>
                  <span v-if="picks[row.file]" class="picked">
                    {{ picks[row.file].name }}
                    <span v-if="progress[row.file] > 0" class="muted">
                      {{ progress[row.file] }}%
                    </span>
                  </span>
                </template>
              </el-table-column>
            </el-table>
          </div>
        </el-form-item>

        <div class="drawer-actions">
          <el-button @click="drawer = false">取消</el-button>
          <el-button type="primary" :loading="publishing" @click="submit">
            上传并发布
          </el-button>
        </div>
      </el-form>
    </el-drawer>
  </div>
</template>

<style scoped>
.release-page {
  padding: 20px;
}
.page-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 20px;
  margin-bottom: 18px;
}
.page-head h2 {
  margin: 0 0 4px;
  font-size: 20px;
}
.page-head p {
  margin: 0;
  max-width: 62ch;
}
.muted {
  color: var(--el-text-color-secondary);
}
.small {
  font-size: 12px;
}
.channel-card h3 {
  margin: 4px 0 2px;
  font-size: 22px;
}
.channel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.notes {
  margin: 10px 0 0;
  white-space: pre-wrap;
  color: var(--el-text-color-regular);
}
.block {
  margin-top: 18px;
}
.file-table {
  margin: 4px 32px;
}
.specs {
  width: 100%;
}
.picked {
  display: inline-block;
  margin-left: 8px;
  font-size: 12px;
  color: var(--el-text-color-regular);
  max-width: 120px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  vertical-align: middle;
}
.drawer-actions {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  padding-top: 8px;
}
.w-full {
  width: 100%;
}
code {
  font-family: var(--el-font-family-mono, monospace);
  font-size: 12px;
}
.hash {
  word-break: break-all;
}
</style>
