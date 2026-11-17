<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  createChangelog,
  deleteChangelog,
  getChangelog,
  updateChangelog,
  type ChangelogEntry
} from "@/api/admin";

defineOptions({ name: "ChangelogManage" });

const loading = ref(false);
const saving = ref(false);
const dialog = ref(false);
const editingId = ref<number | null>(null);
const rows = ref<ChangelogEntry[]>([]);

const form = reactive({
  version: "",
  channel: "stable",
  severity: "recommended",
  title: "",
  body: "",
  publishedAt: ""
});

function resetForm() {
  form.version = "";
  form.channel = "stable";
  form.severity = "recommended";
  form.title = "";
  form.body = "";
  form.publishedAt = new Date().toISOString().slice(0, 10);
  editingId.value = null;
}

function severityLabel(severity: string) {
  if (severity === "required") return "强制更新";
  if (severity === "recommended") return "建议更新";
  return severity;
}

function severityType(severity: string) {
  return severity === "required" ? "warning" : "success";
}

function shortDate(at: string) {
  return at ? at.slice(0, 10) : "";
}

async function load() {
  loading.value = true;
  try {
    const result = await getChangelog();
    rows.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "读取更新记录失败");
  } finally {
    loading.value = false;
  }
}

function openCreate() {
  resetForm();
  dialog.value = true;
}

function openEdit(row: ChangelogEntry) {
  editingId.value = row.id;
  form.version = row.version;
  form.channel = row.channel;
  form.severity = row.severity;
  form.title = row.title;
  form.body = row.body;
  form.publishedAt = shortDate(row.publishedAt) || new Date().toISOString().slice(0, 10);
  dialog.value = true;
}

async function save() {
  if (!/^[A-Za-z0-9.+-]+$/.test(form.version.trim())) {
    ElMessage.warning("版本号只能用字母、数字、点、加号和短横线");
    return;
  }
  saving.value = true;
  try {
    const payload = {
      version: form.version.trim(),
      channel: form.channel,
      severity: form.severity,
      title: form.title,
      body: form.body,
      publishedAt: form.publishedAt
    };
    if (editingId.value === null) {
      await createChangelog(payload);
      ElMessage.success("已新增更新记录");
    } else {
      await updateChangelog(editingId.value, payload);
      ElMessage.success("已保存");
    }
    dialog.value = false;
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function remove(row: ChangelogEntry) {
  try {
    await ElMessageBox.confirm(
      `删除 v${row.version} 的更新记录？官网 /changelog 上的这一条会立即消失。`,
      "删除更新记录",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await deleteChangelog(row.id);
    ElMessage.success("已删除");
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "删除失败");
  }
}

onMounted(load);
</script>

<template>
  <div class="changelog-page">
    <div class="page-head">
      <div>
        <h2>更新记录</h2>
        <p class="muted">
          这里的内容就是官网
          <a href="/changelog" target="_blank" rel="noopener">/changelog</a>
          页面；发布版本时会自动写入一条。
        </p>
      </div>
      <el-button v-perms="'changelog:write'" type="primary" @click="openCreate">
        新增记录
      </el-button>
    </div>

    <el-table v-loading="loading" :data="rows" size="small">
      <el-table-column label="版本" min-width="150">
        <template #default="{ row }">
          <div class="version">v{{ row.version }}</div>
          <div v-if="row.title" class="muted small">{{ row.title }}</div>
        </template>
      </el-table-column>
      <el-table-column label="通道" width="100">
        <template #default="{ row }">
          <el-tag :type="row.channel === 'beta' ? 'info' : 'primary'" size="small">
            {{ row.channel }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="更新分级" width="120">
        <template #default="{ row }">
          <el-tag :type="severityType(row.severity)" size="small">{{ severityLabel(row.severity) }}</el-tag>
        </template>
      </el-table-column>
      <el-table-column label="日期" width="140">
        <template #default="{ row }">{{ shortDate(row.publishedAt) }}</template>
      </el-table-column>
      <el-table-column label="操作" width="160" fixed="right">
        <template #default="{ row }">
          <el-button v-perms="'changelog:write'" link type="primary" @click="openEdit(row as ChangelogEntry)">
            编辑
          </el-button>
          <el-button v-perms="'changelog:write'" link type="danger" @click="remove(row as ChangelogEntry)">
            删除
          </el-button>
        </template>
      </el-table-column>
    </el-table>

    <el-dialog
      v-model="dialog"
      :title="editingId === null ? '新增更新记录' : '编辑更新记录'"
      width="720px"
      destroy-on-close
    >
      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :span="12">
            <el-form-item label="版本号">
              <el-input v-model="form.version" placeholder="0.1.20" />
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="通道">
              <el-select v-model="form.channel" class="w-full">
                <el-option label="stable" value="stable" />
                <el-option label="beta" value="beta" />
              </el-select>
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
            <el-form-item label="日期">
              <el-date-picker
                v-model="form.publishedAt"
                type="date"
                value-format="YYYY-MM-DD"
                class="w-full"
              />
            </el-form-item>
          </el-col>
        </el-row>
        <el-form-item label="小标题（可选）">
          <el-input v-model="form.title" placeholder="例如：终端分栏与 SFTP 断点续传" />
        </el-form-item>
        <el-form-item label="正文（Markdown）">
          <el-input v-model="form.body" type="textarea" :rows="12" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="save">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.changelog-page {
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
.version {
  font-weight: 600;
}
.w-full {
  width: 100%;
}
</style>
