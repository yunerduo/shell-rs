<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { ElMessage } from "element-plus";
import {
  getAiSettings,
  getAiUsage,
  saveAiSettings,
  setAiUserEnabled,
  testAiUpstream,
  type AiSettingsPayload,
  type AiTestResult,
  type AiUsageRow
} from "@/api/admin";
import { formatFullDateTime, formatShortDateTime } from "@/utils/datetime";

defineOptions({ name: "AiTrial" });

interface Copy {
  title: string;
  subtitle: string;
  language: string;
  enabled: string;
  enabledHint: string;
  upstream: string;
  baseUrl: string;
  baseUrlHint: string;
  apiKey: string;
  apiKeyKeep: string;
  apiKeySet: string;
  clearKey: string;
  model: string;
  modelHint: string;
  limits: string;
  maxContext: string;
  maxContextHint: string;
  maxOutput: string;
  maxOutputHint: string;
  inputLimit: string;
  inputLimitHint: string;
  temperature: string;
  temperatureHint: string;
  useTemperature: string;
  topP: string;
  topPHint: string;
  useTopP: string;
  samplingBoth: string;
  samplingBothHint: string;
  streamIncludeUsage: string;
  streamIncludeUsageHint: string;
  timeout: string;
  timeoutHint: string;
  connectTimeout: string;
  connectTimeoutHint: string;
  retry: string;
  retryHint: string;
  retryBackoff: string;
  backoffHint: string;
  customHeaders: string;
  customHeadersHint: string;
  customHeadersInvalid: string;
  globalDailyTokens: string;
  globalDailyTokensHint: string;
  minInterval: string;
  minIntervalScope: string;
  scopeUser: string;
  scopeEmail: string;
  intervalHint: string;
  readableHint: string;
  readableUpstream: string;
  readonlyHint: string;
  dailyRequests: string;
  dailyRequestsHint: string;
  dailyTokens: string;
  dailyTokensHint: string;
  concurrency: string;
  concurrencyHint: string;
  maxRequestBytes: string;
  bytesHint: string;
  save: string;
  testing: string;
  test: string;
  testTitle: string;
  refreshed: string;
  saved: string;
  saveFailed: string;
  loadFailed: string;
  switchFailed: string;
  usage: string;
  usageHint: string;
  search: string;
  user: string;
  account: string;
  aiSwitch: string;
  requestsToday: string;
  tokensToday: string;
  total: string;
  lastUsed: string;
  never: string;
  disabledAccount: string;
  on: string;
  off: string;
  refresh: string;
  categories: Record<string, string>;
}

const zh: Copy = {
  title: "内置 AI · 试用",
  subtitle:
    "服务端代连一个 OpenAI 兼容上游，已登录用户用自己的登录态调用。上游 Key 只存在服务端，任何接口都不会下发。用量只记次数、token 与时间，不存对话正文。",
  language: "语言",
  enabled: "启用内置 AI 试用",
  enabledHint: "关闭后所有用户的请求都会收到 403，其他功能不受影响。",
  upstream: "上游",
  baseUrl: "上游地址（base URL）",
  baseUrlHint: "例如 https://api.example.com/v1，保存后立即生效。",
  apiKey: "上游 API Key",
  apiKeyKeep: "已保存，留空表示不修改",
  apiKeySet: "还没有设置，填写后保存",
  clearKey: "清除已保存的 Key",
  model: "模型 id",
  modelHint: "用户只能用到这一个模型，传其他模型会被拒绝。",
  limits: "限额与防护",
  maxContext: "上下文窗口（token）",
  maxContextHint: "128k = 131072。客户端按它算使用率。",
  maxOutput: "最大输出（token）",
  maxOutputHint: "默认 4096；客户端传得再大也会被夹到这个值。",
  inputLimit: "单请求最大输入（token）",
  inputLimitHint: "默认 122880；不能超过上下文窗口。",
  temperature: "Temperature",
  temperatureHint: "默认不设置，沿用客户端传入值；范围 0–2。",
  useTemperature: "使用自定义 Temperature",
  topP: "Top P",
  topPHint: "默认不设置；范围 0–1。",
  useTopP: "使用自定义 Top P",
  samplingBoth: "同时传 Temperature 与 Top P",
  samplingBothHint:
    "默认关闭。部分模型不接受两者同时出现；关闭时只传 Temperature（有 Top P 也优先保留 Temperature）。",
  streamIncludeUsage: "流式请求补 include_usage",
  streamIncludeUsageHint:
    "默认开启。开启时向上游补 stream_options.include_usage，计量用上游返回的真实 token；关闭则按文本估算。",
  timeout: "整体超时（秒）",
  timeoutHint: "默认 60 秒；连接成功后的总时长上限。",
  connectTimeout: "连接超时（秒）",
  connectTimeoutHint: "默认 10 秒。",
  retry: "失败重试次数（次）",
  retryHint: "默认 1 次；只重试连接失败和上游 5xx。",
  retryBackoff: "重试退避（毫秒）",
  backoffHint: "默认 300 毫秒；按 2 的幂递增，最多 10 秒；0 表示立即重试。",
  customHeaders: "自定义请求头（JSON 对象）",
  customHeadersHint:
    '例如 {"X-Title":"ShellAs"}。只允许非敏感头，Authorization、Cookie、含 key/token/secret 的头会被拒绝。',
  customHeadersInvalid: "自定义请求头必须是 JSON 对象，且值都是字符串",
  globalDailyTokens: "全站每日 token 上限（token）",
  globalDailyTokensHint: "默认 0（不限）；按当天全站输入 + 输出 token 计。",
  minInterval: "最小请求间隔（秒）",
  minIntervalScope: "间隔按什么计",
  scopeUser: "按用户",
  scopeEmail: "按邮箱",
  intervalHint: "默认 1 秒；0 表示不限；超限返回 429 too_frequent。",
  readableHint: "默认开启。上游失败时把上游的一句话原因带给客户端（已脱敏、截断）。",
  readableUpstream: "可读的上游错误提示",
  readonlyHint: "连接超时按实际秒数缓存连接池，保存后下一条请求生效。",
  dailyRequests: "每用户每日请求上限（次）",
  dailyRequestsHint: "默认 200 次；0 表示不限。",
  dailyTokens: "每用户每日 token 上限（token）",
  dailyTokensHint: "默认 0（不限）。",
  concurrency: "每用户并发上限（个）",
  concurrencyHint: "默认 2 个；超限返回 429 too_concurrent。",
  maxRequestBytes: "请求体上限（字节）",
  bytesHint: "范围 4 KiB – 1 MiB，默认 262144。",
  save: "保存并立即生效",
  testing: "测试中…",
  test: "测试上游",
  testTitle: "上游自检",
  refreshed: "已刷新",
  saved: "已保存，下一条请求立即生效",
  saveFailed: "保存失败",
  loadFailed: "读取配置失败",
  switchFailed: "调整开关失败",
  usage: "用户用量",
  usageHint: "按最近使用时间排序，只有计数，没有对话内容。",
  search: "搜索账号或邮箱",
  user: "用户",
  account: "账号状态",
  aiSwitch: "内置 AI",
  requestsToday: "今日请求",
  tokensToday: "今日 token",
  total: "累计请求 / token",
  lastUsed: "最近使用",
  never: "从未使用",
  disabledAccount: "已停用",
  on: "开",
  off: "关",
  refresh: "刷新",
  categories: {
    ok: "上游可用",
    unreachable: "上游不可达",
    timeout: "上游超时",
    auth_failed: "鉴权失败",
    rejected: "上游拒绝",
    upstream_error: "上游错误",
    not_configured: "尚未配置"
  }
};

const en: Copy = {
  title: "Built-in AI · Trial",
  subtitle:
    "The server proxies one OpenAI-compatible upstream. Signed-in users call it with their own session; the upstream key never leaves the server. Usage stores counts, tokens and timestamps only — never conversation content.",
  language: "Language",
  enabled: "Enable built-in AI trial",
  enabledHint: "When off, every user request gets 403; all other features keep working.",
  upstream: "Upstream",
  baseUrl: "Upstream base URL",
  baseUrlHint: "For example https://api.example.com/v1. Changes apply immediately.",
  apiKey: "Upstream API key",
  apiKeyKeep: "Already stored; leave blank to keep it",
  apiKeySet: "Not set yet",
  clearKey: "Clear the stored key",
  model: "Model id",
  modelHint: "Users can only use this model; other ids are rejected.",
  limits: "Limits & protection",
  maxContext: "Context window (tokens)",
  maxContextHint: "128k = 131072. Clients use it to compute usage.",
  maxOutput: "Max output (tokens)",
  maxOutputHint: "Default 4096; larger client values are clamped to it.",
  inputLimit: "Max input per request (tokens)",
  inputLimitHint: "Default 122880; cannot exceed the context window.",
  temperature: "Temperature",
  temperatureHint: "Unset by default (client value is used); range 0–2.",
  useTemperature: "Use a custom temperature",
  topP: "Top P",
  topPHint: "Unset by default; range 0–1.",
  useTopP: "Use a custom top_p",
  samplingBoth: "Send temperature and top_p together",
  samplingBothHint:
    "Off by default. Some models reject sending both. When off, only temperature is sent (temperature wins if both are set).",
  streamIncludeUsage: "Add include_usage to streamed requests",
  streamIncludeUsageHint:
    "On by default. Adds stream_options.include_usage so usage uses upstream token counts; when off, tokens are estimated from text.",
  timeout: "Overall timeout (seconds)",
  timeoutHint: "Default 60 s; total time allowed after connecting.",
  connectTimeout: "Connect timeout (seconds)",
  connectTimeoutHint: "Default 10 s.",
  retry: "Retries on failure",
  retryHint: "Default 1; only connection failures and upstream 5xx are retried.",
  retryBackoff: "Retry backoff (ms)",
  backoffHint: "Default 300 ms; doubles per attempt, capped at 10 s; 0 retries immediately.",
  customHeaders: "Custom request headers (JSON object)",
  customHeadersHint:
    'For example {"X-Title":"ShellAs"}. Only non-sensitive headers; Authorization, Cookie and names containing key/token/secret are rejected.',
  customHeadersInvalid: "Custom headers must be a JSON object of string values",
  globalDailyTokens: "Global daily token limit (tokens)",
  globalDailyTokensHint:
    "Default 0 (unlimited); counts all input + output tokens for the day.",
  minInterval: "Minimum interval (seconds)",
  minIntervalScope: "Interval counted per",
  scopeUser: "User",
  scopeEmail: "Email",
  intervalHint: "Default 1 s; 0 means unlimited; over the limit returns 429 too_frequent.",
  readableHint:
    "On by default. Pass a one-line upstream reason to clients on failure (redacted and truncated).",
  readableUpstream: "Readable upstream error hint",
  readonlyHint:
    "Connection pools are cached per timeout value; the next request uses the new one.",
  dailyRequests: "Daily requests per user",
  dailyRequestsHint: "Default 200; 0 means unlimited.",
  dailyTokens: "Daily tokens per user",
  dailyTokensHint: "Default 0 (unlimited).",
  concurrency: "Concurrent requests per user",
  concurrencyHint: "Default 2; over the limit returns 429 too_concurrent.",
  maxRequestBytes: "Max request body (bytes)",
  bytesHint: "4 KiB – 1 MiB, default 262144.",
  save: "Save and apply",
  testing: "Testing…",
  test: "Test upstream",
  testTitle: "Upstream self-check",
  refreshed: "Refreshed",
  saved: "Saved; the next request uses it",
  saveFailed: "Save failed",
  loadFailed: "Failed to load settings",
  switchFailed: "Failed to change the switch",
  usage: "User usage",
  usageHint: "Ordered by last use. Counters only, no message content.",
  search: "Search username or email",
  user: "User",
  account: "Account",
  aiSwitch: "Built-in AI",
  requestsToday: "Requests today",
  tokensToday: "Tokens today",
  total: "Total requests / tokens",
  lastUsed: "Last used",
  never: "Never",
  disabledAccount: "Disabled",
  on: "On",
  off: "Off",
  refresh: "Refresh",
  categories: {
    ok: "Upstream reachable",
    unreachable: "Upstream unreachable",
    timeout: "Upstream timed out",
    auth_failed: "Authentication failed",
    rejected: "Upstream rejected",
    upstream_error: "Upstream error",
    not_configured: "Not configured"
  }
};

const lang = ref<"zh" | "en">(
  localStorage.getItem("shellas_ai_lang") === "en" ? "en" : "zh"
);
const copy = computed(() => (lang.value === "zh" ? zh : en));

const loading = ref(false);
const saving = ref(false);
const testing = ref(false);
const testResult = ref<AiTestResult | null>(null);
const hasApiKey = ref(false);
const customTemperature = ref(false);
const customTopP = ref(false);
const customHeadersText = ref("{}");

const cfg = ref({
  enabled: false,
  baseUrl: "",
  model: "",
  apiKey: "",
  clearApiKey: false,
  maxContextTokens: 131072,
  maxOutputTokens: 4096,
  maxInputTokens: 122880,
  temperature: 0.7,
  topP: 0.9,
  samplingBoth: false,
  streamIncludeUsage: true,
  timeoutSeconds: 60,
  connectTimeoutSeconds: 10,
  retryCount: 1,
  retryBackoffMs: 300,
  dailyRequestLimit: 200,
  dailyTokenLimit: 0,
  globalDailyTokenLimit: 0,
  maxConcurrentPerUser: 2,
  maxRequestBytes: 262144,
  minIntervalSeconds: 1,
  minIntervalScope: "user" as "user" | "email",
  readableUpstreamHint: true
});

const usageRows = ref<AiUsageRow[]>([]);
const usageTotal = ref(0);
const page = ref(1);
const pageSize = ref(20);
const search = ref("");
const usageLoading = ref(false);

function rememberLang(value: "zh" | "en") {
  lang.value = value;
  localStorage.setItem("shellas_ai_lang", value);
}

async function load() {
  loading.value = true;
  try {
    const result = await getAiSettings();
    const data = result.data;
    cfg.value.enabled = data.enabled;
    cfg.value.baseUrl = data.baseUrl;
    cfg.value.model = data.model;
    cfg.value.apiKey = "";
    cfg.value.clearApiKey = false;
    cfg.value.maxContextTokens = data.maxContextTokens;
    cfg.value.maxOutputTokens = data.maxOutputTokens;
    cfg.value.maxInputTokens = data.maxInputTokens;
    customTemperature.value = data.temperature !== null;
    cfg.value.temperature = data.temperature ?? 0.7;
    customTopP.value = data.topP !== null;
    cfg.value.topP = data.topP ?? 0.9;
    cfg.value.samplingBoth = data.samplingBoth;
    cfg.value.streamIncludeUsage = data.streamIncludeUsage;
    cfg.value.timeoutSeconds = data.timeoutSeconds;
    cfg.value.connectTimeoutSeconds = data.connectTimeoutSeconds;
    cfg.value.retryCount = data.retryCount;
    cfg.value.retryBackoffMs = data.retryBackoffMs;
    customHeadersText.value = JSON.stringify(data.customHeaders ?? {}, null, 2);
    cfg.value.dailyRequestLimit = data.dailyRequestLimit;
    cfg.value.dailyTokenLimit = data.dailyTokenLimit;
    cfg.value.globalDailyTokenLimit = data.globalDailyTokenLimit;
    cfg.value.maxConcurrentPerUser = data.maxConcurrentPerUser;
    cfg.value.maxRequestBytes = data.maxRequestBytes;
    cfg.value.minIntervalSeconds = data.minIntervalSeconds;
    cfg.value.minIntervalScope = data.minIntervalScope;
    cfg.value.readableUpstreamHint = data.readableUpstreamHint;
    hasApiKey.value = data.hasApiKey;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || copy.value.loadFailed);
  } finally {
    loading.value = false;
  }
}

async function save() {
  let headers: Record<string, string>;
  try {
    const parsed = JSON.parse(customHeadersText.value || "{}");
    if (
      !parsed ||
      typeof parsed !== "object" ||
      Array.isArray(parsed) ||
      Object.values(parsed).some(value => typeof value !== "string")
    ) {
      throw new Error("invalid");
    }
    headers = parsed as Record<string, string>;
  } catch {
    ElMessage.error(copy.value.customHeadersInvalid);
    return;
  }
  saving.value = true;
  try {
    const payload: AiSettingsPayload = {
      enabled: cfg.value.enabled,
      baseUrl: cfg.value.baseUrl,
      model: cfg.value.model,
      maxContextTokens: cfg.value.maxContextTokens,
      maxOutputTokens: cfg.value.maxOutputTokens,
      maxInputTokens: cfg.value.maxInputTokens,
      samplingBoth: cfg.value.samplingBoth,
      streamIncludeUsage: cfg.value.streamIncludeUsage,
      timeoutSeconds: cfg.value.timeoutSeconds,
      connectTimeoutSeconds: cfg.value.connectTimeoutSeconds,
      retryCount: cfg.value.retryCount,
      retryBackoffMs: cfg.value.retryBackoffMs,
      customHeaders: headers,
      dailyRequestLimit: cfg.value.dailyRequestLimit,
      dailyTokenLimit: cfg.value.dailyTokenLimit,
      globalDailyTokenLimit: cfg.value.globalDailyTokenLimit,
      maxConcurrentPerUser: cfg.value.maxConcurrentPerUser,
      maxRequestBytes: cfg.value.maxRequestBytes,
      minIntervalSeconds: cfg.value.minIntervalSeconds,
      minIntervalScope: cfg.value.minIntervalScope,
      readableUpstreamHint: cfg.value.readableUpstreamHint
    };
    if (cfg.value.apiKey.trim()) payload.apiKey = cfg.value.apiKey.trim();
    if (cfg.value.clearApiKey) payload.clearApiKey = true;
    if (customTemperature.value) {
      payload.temperature = cfg.value.temperature;
    } else {
      payload.clearTemperature = true;
    }
    if (customTopP.value) {
      payload.topP = cfg.value.topP;
    } else {
      payload.clearTopP = true;
    }
    const result = await saveAiSettings(payload);
    hasApiKey.value = result.data.hasApiKey;
    cfg.value.apiKey = "";
    cfg.value.clearApiKey = false;
    ElMessage.success(copy.value.saved);
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || copy.value.saveFailed);
  } finally {
    saving.value = false;
  }
}

async function runTest() {
  testing.value = true;
  testResult.value = null;
  try {
    const result = await testAiUpstream();
    testResult.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || copy.value.saveFailed);
  } finally {
    testing.value = false;
  }
}

async function loadUsage() {
  usageLoading.value = true;
  try {
    const result = await getAiUsage({
      page: page.value,
      pageSize: pageSize.value,
      search: search.value.trim()
    });
    usageRows.value = result.data.items;
    usageTotal.value = result.data.total;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || copy.value.loadFailed);
  } finally {
    usageLoading.value = false;
  }
}

async function toggleAi(row: AiUsageRow) {
  try {
    await setAiUserEnabled(row.userId, row.aiEnabled);
    ElMessage.success(copy.value.saved);
  } catch (error: any) {
    row.aiEnabled = !row.aiEnabled;
    ElMessage.error(error?.response?.data?.message || copy.value.switchFailed);
  }
}

function categoryType(category: AiTestResult["category"]) {
  if (category === "ok") return "success";
  if (category === "auth_failed" || category === "upstream_error") return "danger";
  return "warning";
}

function formatNumber(value: number) {
  return value.toLocaleString();
}

function formatTime(value: string | null) {
  if (!value) return copy.value.never;
  return formatShortDateTime(value);
}

function onSearch() {
  page.value = 1;
  loadUsage();
}

type TextKey = "baseUrl" | "model" | "apiKey";
type NumberKey =
  | "maxContextTokens"
  | "maxOutputTokens"
  | "maxInputTokens"
  | "temperature"
  | "topP"
  | "timeoutSeconds"
  | "connectTimeoutSeconds"
  | "retryCount"
  | "retryBackoffMs"
  | "dailyRequestLimit"
  | "dailyTokenLimit"
  | "globalDailyTokenLimit"
  | "minIntervalSeconds"
  | "maxConcurrentPerUser"
  | "maxRequestBytes";

function setText(key: TextKey, value: string | number | null | undefined) {
  cfg.value[key] = value == null ? "" : String(value);
}

function setNumber(key: NumberKey, value: number | null | undefined) {
  if (typeof value === "number") cfg.value[key] = value;
}

function setFlag(key: "clearApiKey", value: boolean | string | number) {
  cfg.value[key] = value === true;
}

onMounted(() => {
  load();
  loadUsage();
});
</script>

<template>
  <div v-loading="loading" class="ai-page">
    <div class="page-head">
      <div>
        <h2>{{ copy.title }}</h2>
        <p class="muted">{{ copy.subtitle }}</p>
      </div>
      <el-radio-group
        :model-value="lang"
        size="small"
        @change="rememberLang"
      >
        <el-radio-button value="zh">中文</el-radio-button>
        <el-radio-button value="en">English</el-radio-button>
      </el-radio-group>
    </div>

    <el-card shadow="never" class="block">
      <template #header>
        <div class="card-head">
          <span class="card-title">{{ copy.upstream }}</span>
          <el-switch
            v-model="cfg.enabled"
            :active-text="copy.on"
            :inactive-text="copy.off"
          />
        </div>
      </template>
      <p class="muted small hint">{{ copy.enabledHint }}</p>

      <el-form label-position="top" class="form-grid">
        <el-form-item :label="copy.baseUrl" class="wide">
          <el-input
            :model-value="cfg.baseUrl"
            placeholder="https://api.example.com/v1"
            @update:model-value="value => setText('baseUrl', value)"
          />
          <span class="muted small">{{ copy.baseUrlHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.model">
          <el-input
            :model-value="cfg.model"
            @update:model-value="value => setText('model', value)"
          />
          <span class="muted small">{{ copy.modelHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.apiKey">
          <el-input
            :model-value="cfg.apiKey"
            type="password"
            show-password
            autocomplete="new-password"
            :placeholder="hasApiKey ? copy.apiKeyKeep : copy.apiKeySet"
            @update:model-value="value => setText('apiKey', value)"
          />
          <el-checkbox
            :model-value="cfg.clearApiKey"
            class="clear-key"
            @update:model-value="value => setFlag('clearApiKey', value)"
          >
            {{ copy.clearKey }}
          </el-checkbox>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never" class="block">
      <template #header>
        <span class="card-title">{{ copy.limits }}</span>
      </template>
      <el-form label-position="top" class="form-grid">
        <el-form-item :label="copy.maxContext">
          <el-input-number
            :model-value="cfg.maxContextTokens as number"
            :min="1024"
            :max="2000000"
            :step="1024"
            @update:model-value="value => setNumber('maxContextTokens', value)"
          />
          <span class="muted small">{{ copy.maxContextHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.maxOutput">
          <el-input-number
            :model-value="cfg.maxOutputTokens"
            :min="1"
            :max="200000"
            :step="256"
            @update:model-value="value => setNumber('maxOutputTokens', value)"
          />
          <span class="muted small">{{ copy.maxOutputHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.inputLimit">
          <el-input-number
            :model-value="cfg.maxInputTokens"
            :min="1"
            :max="2000000"
            :step="1024"
            @update:model-value="value => setNumber('maxInputTokens', value)"
          />
          <span class="muted small">{{ copy.inputLimitHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.temperature">
          <div class="temperature">
            <el-switch v-model="customTemperature" :active-text="copy.useTemperature" />
            <el-input-number
              v-if="customTemperature"
              :model-value="cfg.temperature"
              :min="0"
              :max="2"
              :step="0.1"
              :precision="2"
              @update:model-value="value => setNumber('temperature', value)"
            />
          </div>
          <span class="muted small">{{ copy.temperatureHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.topP">
          <div class="temperature">
            <el-switch v-model="customTopP" :active-text="copy.useTopP" />
            <el-input-number
              v-if="customTopP"
              :model-value="cfg.topP"
              :min="0"
              :max="1"
              :step="0.05"
              :precision="2"
              @update:model-value="value => setNumber('topP', value)"
            />
          </div>
          <span class="muted small">{{ copy.topPHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.samplingBoth" class="wide">
          <div class="temperature">
            <el-switch v-model="cfg.samplingBoth" />
            <span class="muted small">{{ copy.samplingBothHint }}</span>
          </div>
        </el-form-item>
        <el-form-item :label="copy.streamIncludeUsage" class="wide">
          <div class="temperature">
            <el-switch v-model="cfg.streamIncludeUsage" />
            <span class="muted small">{{ copy.streamIncludeUsageHint }}</span>
          </div>
        </el-form-item>
        <el-form-item :label="copy.timeout">
          <el-input-number
            :model-value="cfg.timeoutSeconds"
            :min="5"
            :max="600"
            @update:model-value="value => setNumber('timeoutSeconds', value)"
          />
          <span class="muted small">{{ copy.timeoutHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.connectTimeout">
          <el-input-number
            :model-value="cfg.connectTimeoutSeconds"
            :min="1"
            :max="60"
            @update:model-value="value => setNumber('connectTimeoutSeconds', value)"
          />
          <span class="muted small">{{ copy.connectTimeoutHint }} {{ copy.readonlyHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.retry">
          <el-input-number
            :model-value="cfg.retryCount"
            :min="0"
            :max="3"
            @update:model-value="value => setNumber('retryCount', value)"
          />
          <span class="muted small">{{ copy.retryHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.retryBackoff">
          <el-input-number
            :model-value="cfg.retryBackoffMs"
            :min="0"
            :max="5000"
            :step="100"
            @update:model-value="value => setNumber('retryBackoffMs', value)"
          />
          <span class="muted small">{{ copy.backoffHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.dailyRequests">
          <el-input-number
            :model-value="cfg.dailyRequestLimit"
            :min="0"
            :max="100000"
            @update:model-value="value => setNumber('dailyRequestLimit', value)"
          />
          <span class="muted small">{{ copy.dailyRequestsHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.dailyTokens">
          <el-input-number
            :model-value="cfg.dailyTokenLimit"
            :min="0"
            :max="100000000"
            :step="100000"
            @update:model-value="value => setNumber('dailyTokenLimit', value)"
          />
          <span class="muted small">{{ copy.dailyTokensHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.globalDailyTokens">
          <el-input-number
            :model-value="cfg.globalDailyTokenLimit"
            :min="0"
            :max="1000000000"
            :step="100000"
            @update:model-value="value => setNumber('globalDailyTokenLimit', value)"
          />
          <span class="muted small">{{ copy.globalDailyTokensHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.minInterval">
          <el-input-number
            :model-value="cfg.minIntervalSeconds"
            :min="0"
            :max="3600"
            @update:model-value="value => setNumber('minIntervalSeconds', value)"
          />
          <span class="muted small">{{ copy.intervalHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.minIntervalScope">
          <el-select v-model="cfg.minIntervalScope" class="scope-select">
            <el-option :label="copy.scopeUser" value="user" />
            <el-option :label="copy.scopeEmail" value="email" />
          </el-select>
          <span class="muted small">{{ copy.intervalHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.concurrency">
          <el-input-number
            :model-value="cfg.maxConcurrentPerUser"
            :min="1"
            :max="100"
            @update:model-value="value => setNumber('maxConcurrentPerUser', value)"
          />
          <span class="muted small">{{ copy.concurrencyHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.maxRequestBytes">
          <el-input-number
            :model-value="cfg.maxRequestBytes"
            :min="4096"
            :max="1048576"
            :step="4096"
            @update:model-value="value => setNumber('maxRequestBytes', value)"
          />
          <span class="muted small">{{ copy.bytesHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.customHeaders" class="wide">
          <el-input
            v-model="customHeadersText"
            type="textarea"
            :rows="3"
            spellcheck="false"
            placeholder='{"X-Title": "ShellAs"}'
          />
          <span class="muted small">{{ copy.customHeadersHint }}</span>
        </el-form-item>
        <el-form-item :label="copy.readableUpstream" class="wide">
          <div class="temperature">
            <el-switch v-model="cfg.readableUpstreamHint" />
            <span class="muted small">{{ copy.readableHint }}</span>
          </div>
        </el-form-item>
      </el-form>

      <div class="footer-actions">
        <el-button type="primary" :loading="saving" @click="save">{{ copy.save }}</el-button>
        <el-button :loading="testing" @click="runTest">
          {{ testing ? copy.testing : copy.test }}
        </el-button>
        <div v-if="testResult" class="test-result">
          <el-tag :type="categoryType(testResult.category)" size="small">
            {{ copy.categories[testResult.category] || testResult.category }}
          </el-tag>
          <span class="muted small">
            {{ testResult.detail }} · {{ testResult.latencyMs }} ms
          </span>
        </div>
      </div>
    </el-card>

    <el-card shadow="never" class="block">
      <template #header>
        <div class="card-head">
          <div>
            <span class="card-title">{{ copy.usage }}</span>
            <p class="muted small hint">{{ copy.usageHint }}</p>
          </div>
          <div class="actions">
            <el-input
              v-model="search"
              class="search"
              :placeholder="copy.search"
              clearable
              @keyup.enter="onSearch"
              @clear="onSearch"
            />
            <el-button @click="onSearch">{{ copy.refresh }}</el-button>
          </div>
        </div>
      </template>

      <el-table v-loading="usageLoading" :data="usageRows" size="small">
        <el-table-column prop="username" :label="copy.user" min-width="150">
          <template #default="{ row }">
            <span>{{ row.username }}</span>
            <el-tag v-if="row.accountDisabled" type="info" size="small" class="inline-tag">
              {{ copy.disabledAccount }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column :label="copy.aiSwitch" width="110">
          <template #default="{ row }">
            <el-switch v-model="row.aiEnabled" @change="toggleAi(row as AiUsageRow)" />
          </template>
        </el-table-column>
        <el-table-column :label="copy.requestsToday" width="110">
          <template #default="{ row }">{{ formatNumber(row.requestsToday) }}</template>
        </el-table-column>
        <el-table-column :label="copy.tokensToday" width="120">
          <template #default="{ row }">{{ formatNumber(row.tokensToday) }}</template>
        </el-table-column>
        <el-table-column :label="copy.total" min-width="160">
          <template #default="{ row }">
            {{ formatNumber(row.totalRequests) }} /
            {{ formatNumber(row.totalInputTokens + row.totalOutputTokens) }}
          </template>
        </el-table-column>
        <el-table-column :label="copy.lastUsed" min-width="180">
          <template #default="{ row }">
            <el-tooltip
              v-if="row.lastUsedAt"
              :content="formatFullDateTime(row.lastUsedAt)"
              placement="top"
            >
              <span>{{ formatTime(row.lastUsedAt) }}</span>
            </el-tooltip>
            <span v-else>{{ copy.never }}</span>
          </template>
        </el-table-column>
      </el-table>

      <div class="pager">
        <el-pagination
          v-model:current-page="page"
          v-model:page-size="pageSize"
          :total="usageTotal"
          :page-sizes="[20, 50, 100]"
          layout="total, sizes, prev, pager, next"
          @current-change="loadUsage"
          @size-change="onSearch"
        />
      </div>
    </el-card>
  </div>
</template>

<style scoped>
.ai-page {
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
  margin: 0 0 6px;
  font-size: 20px;
}
.page-head p {
  margin: 0;
  max-width: 78ch;
}
.block {
  margin-bottom: 16px;
}
.card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}
.card-title {
  font-weight: 600;
}
.form-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0 24px;
}
.form-grid .wide {
  grid-column: 1 / -1;
}
.hint {
  margin: 0 0 8px;
}
.temperature {
  display: flex;
  align-items: center;
  gap: 12px;
}
.scope-select {
  width: 100%;
}
.clear-key {
  margin-top: 6px;
}
.footer-actions {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
  margin-top: 8px;
}
.test-result {
  display: flex;
  align-items: center;
  gap: 8px;
}
.actions {
  display: flex;
  align-items: center;
  gap: 10px;
}
.search {
  width: 220px;
}
.inline-tag {
  margin-left: 8px;
}
.pager {
  display: flex;
  justify-content: flex-end;
  margin-top: 12px;
}
.muted {
  color: var(--el-text-color-secondary);
}
.small {
  font-size: 12px;
}
</style>
