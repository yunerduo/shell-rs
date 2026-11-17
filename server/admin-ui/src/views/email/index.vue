<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { ElMessage } from "element-plus";
import {
  getEmailSends,
  getEmailSettings,
  getEmailTemplates,
  previewEmailTemplate,
  saveEmailSettings,
  saveEmailTemplate,
  sendEmailTest,
  type EmailPreview,
  type EmailSendRow,
  type EmailSettings,
  type EmailSettingsPayload,
  type EmailTemplate,
  type EmailTestResult
} from "@/api/admin";

defineOptions({ name: "EmailSettings" });

type Purpose = EmailTemplate["purpose"];

const tab = ref("smtp");
const loading = ref(false);
const saving = ref(false);

const settings = reactive({
  enabled: false,
  host: "",
  port: 587,
  encryption: "starttls" as "starttls" | "tls" | "plain",
  username: "",
  hasPassword: false,
  fromAddress: "",
  fromName: "Agree ShellAs",
  requireVerification: false,
  configured: false,
  warning: null as string | null,
  updatedAt: ""
});

const password = ref("");
const clearPassword = ref(false);

const testTo = ref("");
const testing = ref(false);
const testResult = ref<EmailTestResult | null>(null);

const templates = ref<EmailTemplate[]>([]);
const drafts = reactive<Record<Purpose, { subject: string; body: string }>>({
  register_verification: { subject: "", body: "" },
  password_reset: { subject: "", body: "" }
});
const savingTemplate = ref<Purpose | "">("");
const previewVisible = ref(false);
const preview = ref<EmailPreview | null>(null);
const previewTitle = ref("");

const sends = ref<EmailSendRow[]>([]);
const sendsTotal = ref(0);
const sendsLoading = ref(false);
const sendPage = ref(1);
const sendPageSize = 20;

const purposeLabels: Record<string, string> = {
  register_verification: "注册邮箱验证码",
  password_reset: "找回密码",
  test: "测试邮件"
};

const resultLabels: Record<string, string> = {
  ok: "成功",
  unreachable: "连不上",
  auth_failed: "认证失败",
  rejected: "被拒收",
  not_configured: "未配置"
};

function errorMessage(error: any, fallback: string) {
  return error?.response?.data?.message || fallback;
}

function applySettings(data: EmailSettings) {
  Object.assign(settings, data);
  password.value = "";
  clearPassword.value = false;
}

async function loadSettings() {
  loading.value = true;
  try {
    const result = await getEmailSettings();
    applySettings(result.data);
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "读取 SMTP 配置失败"));
  } finally {
    loading.value = false;
  }
}

async function saveSettings() {
  saving.value = true;
  try {
    const payload: EmailSettingsPayload = {
      enabled: settings.enabled,
      host: settings.host.trim(),
      port: settings.port,
      encryption: settings.encryption,
      username: settings.username.trim(),
      fromAddress: settings.fromAddress.trim(),
      fromName: settings.fromName.trim(),
      requireVerification: settings.requireVerification
    };
    if (password.value) {
      payload.password = password.value;
    }
    if (clearPassword.value) {
      payload.clearPassword = true;
    }
    const result = await saveEmailSettings(payload);
    applySettings(result.data);
    ElMessage.success("已保存，下一条邮件立即生效");
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "保存失败"));
  } finally {
    saving.value = false;
  }
}

async function runTest() {
  const to = testTo.value.trim();
  if (!to) {
    ElMessage.warning("请填写收件邮箱");
    return;
  }
  testing.value = true;
  testResult.value = null;
  try {
    const result = await sendEmailTest(to);
    testResult.value = result.data;
    if (result.data.ok) {
      ElMessage.success("测试邮件已发出");
    } else {
      ElMessage.warning(`发送失败：${resultLabels[result.data.category]}`);
    }
    await loadSends();
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "测试发送失败"));
  } finally {
    testing.value = false;
  }
}

async function loadTemplates() {
  try {
    const result = await getEmailTemplates();
    templates.value = result.data;
    for (const template of result.data) {
      drafts[template.purpose] = {
        subject: template.subject,
        body: template.body
      };
    }
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "读取邮件模板失败"));
  }
}

async function saveTemplate(purpose: Purpose) {
  const draft = drafts[purpose];
  if (!draft) return;
  savingTemplate.value = purpose;
  try {
    await saveEmailTemplate(purpose, {
      subject: draft.subject,
      body: draft.body
    });
    ElMessage.success("模板已保存，立即生效");
    await loadTemplates();
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "保存模板失败"));
  } finally {
    savingTemplate.value = "";
  }
}

async function openPreview(purpose: Purpose) {
  const draft = drafts[purpose];
  if (!draft) return;
  try {
    const result = await previewEmailTemplate(purpose, draft);
    preview.value = result.data;
    previewTitle.value = `${purposeLabels[purpose]} · 预览`;
    previewVisible.value = true;
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "预览失败"));
  }
}

async function loadSends() {
  sendsLoading.value = true;
  try {
    const result = await getEmailSends({
      limit: sendPageSize,
      offset: (sendPage.value - 1) * sendPageSize
    });
    sends.value = result.data.items;
    sendsTotal.value = result.data.total;
  } catch (error: any) {
    ElMessage.error(errorMessage(error, "读取发送记录失败"));
  } finally {
    sendsLoading.value = false;
  }
}

function changeSendPage(page: number) {
  sendPage.value = page;
  loadSends();
}

function templateHint(purpose: Purpose) {
  return purpose === "register_verification"
    ? "注册时勾选的邮箱验证码；占位符 {username}、{code}、{minutes}。"
    : "找回密码时发送的验证码；占位符 {username}、{code}、{minutes}。";
}

onMounted(async () => {
  await Promise.all([loadSettings(), loadTemplates(), loadSends()]);
});
</script>

<template>
  <div class="email-page">
    <el-tabs v-model="tab">
      <el-tab-pane label="SMTP 配置" name="smtp">
        <el-card shadow="never">
          <template #header>
            <div class="card-header">
              <span>SMTP 配置</span>
              <el-tag :type="settings.configured ? 'success' : 'info'">
                {{ settings.configured ? "已配置" : "未配置完整" }}
              </el-tag>
            </div>
          </template>
          <el-alert
            v-if="settings.warning"
            :title="settings.warning"
            type="warning"
            show-icon
            :closable="false"
            class="mb"
          />
          <el-form label-width="150px" :disabled="loading">
            <el-form-item label="启用邮件">
              <el-switch v-model="settings.enabled" />
              <span class="hint">关闭后所有发信请求都会返回「邮件服务尚未配置」。</span>
            </el-form-item>
            <el-form-item label="注册需验证邮箱">
              <el-switch v-model="settings.requireVerification" />
              <span class="hint">
                只影响新注册；已有账号不会被追溯停用。开启前 SMTP 必须已配置并启用。
              </span>
            </el-form-item>
            <el-form-item label="主机">
              <el-input v-model="settings.host" placeholder="smtp.example.com" />
            </el-form-item>
            <el-form-item label="端口">
              <el-input-number v-model="settings.port" :min="1" :max="65535" />
            </el-form-item>
            <el-form-item label="加密方式">
              <el-select v-model="settings.encryption" style="width: 220px">
                <el-option label="STARTTLS（587）" value="starttls" />
                <el-option label="隐式 TLS（465）" value="tls" />
                <el-option label="明文（不加密）" value="plain" />
              </el-select>
            </el-form-item>
            <el-form-item label="用户名">
              <el-input v-model="settings.username" placeholder="可留空，表示不认证" />
            </el-form-item>
            <el-form-item label="口令">
              <el-input
                v-model="password"
                type="password"
                show-password
                :placeholder="settings.hasPassword ? '已配置，留空表示不修改' : '尚未配置'"
              />
              <el-checkbox v-model="clearPassword" class="ml">
                清除已保存的口令
              </el-checkbox>
            </el-form-item>
            <el-form-item label="发件人地址">
              <el-input v-model="settings.fromAddress" placeholder="no-reply@example.com" />
            </el-form-item>
            <el-form-item label="发件人显示名">
              <el-input v-model="settings.fromName" placeholder="Agree ShellAs" />
            </el-form-item>
            <el-form-item>
              <el-button type="primary" :loading="saving" @click="saveSettings">
                保存并立即生效
              </el-button>
              <span v-if="settings.updatedAt" class="hint ml">
                上次修改：{{ settings.updatedAt }}
              </span>
            </el-form-item>
          </el-form>
        </el-card>

        <el-card shadow="never" class="mt">
          <template #header>发送测试邮件</template>
          <div class="test-row">
            <el-input
              v-model="testTo"
              placeholder="收件邮箱"
              style="max-width: 320px"
              @keyup.enter="runTest"
            />
            <el-button type="primary" :loading="testing" @click="runTest">
              发送测试
            </el-button>
          </div>
          <el-alert
            v-if="testResult"
            class="mt"
            :type="testResult.ok ? 'success' : 'error'"
            :closable="false"
            show-icon
          >
            <template #title>
              {{ testResult.ok ? "发送成功" : `发送失败：${resultLabels[testResult.category]}` }}
              <span class="hint">（{{ testResult.latencyMs }} ms）</span>
            </template>
            <div v-if="testResult.detail">{{ testResult.detail }}</div>
          </el-alert>
        </el-card>
      </el-tab-pane>

      <el-tab-pane label="邮件模板" name="templates">
        <el-card
          v-for="template in templates"
          :key="template.purpose"
          shadow="never"
          class="mb"
        >
          <template #header>
            <div class="card-header">
              <span>{{ purposeLabels[template.purpose] }}</span>
              <el-tag :type="template.configured ? 'success' : 'danger'">
                {{ template.configured ? "已配置" : "未配置" }}
              </el-tag>
            </div>
          </template>
          <el-alert
            v-if="!template.configured"
            title="模板缺失或为空，发信前会被拒绝，不会发出半成品邮件。"
            type="error"
            show-icon
            :closable="false"
            class="mb"
          />
          <el-form label-width="90px">
            <el-form-item label="主题">
              <el-input v-model="drafts[template.purpose].subject" />
            </el-form-item>
            <el-form-item label="正文">
              <el-input
                v-model="drafts[template.purpose].body"
                type="textarea"
                :rows="8"
                resize="vertical"
              />
            </el-form-item>
            <el-form-item>
              <el-button
                :loading="savingTemplate === template.purpose"
                type="primary"
                @click="saveTemplate(template.purpose)"
              >
                保存模板
              </el-button>
              <el-button @click="openPreview(template.purpose)">预览</el-button>
              <span class="hint ml">{{ templateHint(template.purpose) }}</span>
            </el-form-item>
          </el-form>
        </el-card>
      </el-tab-pane>

      <el-tab-pane label="发送记录" name="sends">
        <el-card shadow="never">
          <template #header>
            <div class="card-header">
              <span>发送记录（只存时间、邮箱、用途和结果，不存验证码）</span>
              <el-button :loading="sendsLoading" @click="loadSends">刷新</el-button>
            </div>
          </template>
          <el-table v-loading="sendsLoading" :data="sends" size="small">
            <el-table-column prop="createdAt" label="时间" width="190" />
            <el-table-column prop="email" label="邮箱" min-width="200" />
            <el-table-column label="用途" width="160">
              <template #default="{ row }">
                {{ purposeLabels[row.purpose] || row.purpose }}
              </template>
            </el-table-column>
            <el-table-column label="结果" width="110">
              <template #default="{ row }">
                <el-tag :type="row.result === 'ok' ? 'success' : 'danger'">
                  {{ resultLabels[row.result] || row.result }}
                </el-tag>
              </template>
            </el-table-column>
            <el-table-column prop="detail" label="说明" min-width="240" />
          </el-table>
          <el-pagination
            v-if="sendsTotal > sendPageSize"
            class="mt"
            layout="prev, pager, next"
            :total="sendsTotal"
            :page-size="sendPageSize"
            :current-page="sendPage"
            @current-change="changeSendPage"
          />
        </el-card>
      </el-tab-pane>
    </el-tabs>

    <el-dialog v-model="previewVisible" :title="previewTitle" width="640px">
      <div v-if="preview" class="preview">
        <div class="preview-subject">{{ preview.subject }}</div>
        <pre class="preview-body">{{ preview.body }}</pre>
        <div class="hint">
          示例变量：{{ preview.variables.username }} / {{ preview.variables.code }} /
          {{ preview.variables.minutes }} 分钟
        </div>
      </div>
    </el-dialog>
  </div>
</template>

<style scoped>
.email-page {
  padding: 4px;
}
.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.hint {
  margin-left: 12px;
  color: var(--el-text-color-secondary);
  font-size: 12px;
}
.mb {
  margin-bottom: 16px;
}
.mt {
  margin-top: 16px;
}
.ml {
  margin-left: 12px;
}
.test-row {
  display: flex;
  gap: 12px;
  align-items: center;
}
.preview-subject {
  font-weight: 600;
  margin-bottom: 12px;
}
.preview-body {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  background: var(--el-fill-color-light);
  border-radius: 4px;
  padding: 12px;
  font-family: inherit;
}
</style>
