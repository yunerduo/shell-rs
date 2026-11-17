<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  deleteUser,
  getUsers,
  resetUserPassword,
  updateUser,
  type UserAccount
} from "@/api/admin";
import { formatFullDateTime, formatShortDateTime } from "@/utils/datetime";

defineOptions({ name: "UserAccounts" });

const loading = ref(false);
const saving = ref(false);
const rows = ref<UserAccount[]>([]);
const keyword = ref("");
const editDialog = ref(false);
const editing = ref<UserAccount | null>(null);
const passwordDialog = ref(false);
const passwordTarget = ref<UserAccount | null>(null);

const form = reactive({
  quotaMb: 50,
  disabled: false
});

const passwordForm = reactive({
  password: "",
  confirm: ""
});

const filtered = computed(() => {
  const text = keyword.value.trim().toLowerCase();
  if (!text) return rows.value;
  return rows.value.filter(
    row =>
      row.username.toLowerCase().includes(text) ||
      (row.email ?? "").toLowerCase().includes(text) ||
      (row.phone ?? "").includes(text)
  );
});

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

function quotaMb(row: UserAccount) {
  return Math.max(1, Math.round(row.quotaBytes / 1024 / 1024));
}

function usagePercent(row: UserAccount) {
  if (!row.quotaBytes) return 0;
  return Math.min(100, Math.round((row.usedBytes / row.quotaBytes) * 100));
}

async function load() {
  loading.value = true;
  try {
    const result = await getUsers();
    rows.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "读取用户失败");
  } finally {
    loading.value = false;
  }
}

function openEdit(row: UserAccount) {
  editing.value = row;
  form.quotaMb = quotaMb(row);
  form.disabled = row.disabled;
  editDialog.value = true;
}

async function save() {
  if (!editing.value) return;
  saving.value = true;
  try {
    await updateUser(editing.value.id, {
      quotaMb: form.quotaMb,
      disabled: form.disabled
    });
    ElMessage.success("已保存");
    editDialog.value = false;
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

function openPassword(row: UserAccount) {
  passwordTarget.value = row;
  passwordForm.password = "";
  passwordForm.confirm = "";
  passwordDialog.value = true;
}

async function savePassword() {
  if (!passwordTarget.value) return;
  const password = passwordForm.password;
  const classes = [/[a-z]/, /[A-Z]/, /\d/, /[^a-zA-Z0-9]/].filter(rule =>
    rule.test(password)
  ).length;
  if (password.length < 8 || password.length > 128 || classes < 2) {
    ElMessage.warning("口令要 8–128 位，且至少包含字母、数字、符号中的两类");
    return;
  }
  if (passwordForm.password !== passwordForm.confirm) {
    ElMessage.warning("两次输入的口令不一致");
    return;
  }
  saving.value = true;
  try {
    await resetUserPassword(passwordTarget.value.id, passwordForm.password);
    ElMessage.success("口令已重置，该用户需要重新登录");
    passwordDialog.value = false;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "重置口令失败");
  } finally {
    saving.value = false;
  }
}

async function remove(row: UserAccount) {
  try {
    await ElMessageBox.confirm(
      `删除用户 ${row.username}？云端的 ${row.versions} 个版本会一起删掉，且无法恢复。`,
      "删除用户",
      {
        type: "warning",
        confirmButtonText: "删除",
        cancelButtonText: "取消"
      }
    );
  } catch {
    return;
  }
  try {
    await deleteUser(row.id);
    ElMessage.success("已删除");
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "删除失败");
  }
}

onMounted(load);
</script>

<template>
  <div class="users-page">
    <div class="page-head">
      <div>
        <h2>用户账号</h2>
        <p class="muted">
          客户端用户与后台管理员是两套账号、两套会话。这里只管云同步的配额和账号状态，
          看不到用户的主机与设置内容（服务端存的是密文）。
        </p>
      </div>
      <div class="actions">
        <el-input
          v-model="keyword"
          class="search"
          placeholder="搜索账号、邮箱或手机号"
          clearable
        />
        <el-button @click="load">刷新</el-button>
      </div>
    </div>

    <el-table
      v-loading="loading"
      :data="filtered"
      size="small"
      table-layout="fixed"
      class="users-table"
    >
      <el-table-column prop="username" label="账号" min-width="100" show-overflow-tooltip />
      <el-table-column prop="email" label="邮箱" min-width="120">
        <template #default="{ row }">
          <div class="text-cell">
            <span class="ellipsis" :title="row.email || '—'">{{ row.email || "—" }}</span>
            <el-tag v-if="row.email && !row.emailVerified" size="small" type="info">
              未验证
            </el-tag>
          </div>
        </template>
      </el-table-column>
      <el-table-column prop="phone" label="手机号" min-width="150">
        <template #default="{ row }">
          <div class="text-cell">
            <span class="ellipsis" :title="row.phone || '—'">{{ row.phone || "—" }}</span>
            <el-tag v-if="row.phone && !row.phoneVerified" size="small" type="info">
              未验证
            </el-tag>
          </div>
        </template>
      </el-table-column>
      <el-table-column label="状态" width="70" align="center">
        <template #default="{ row }">
          <el-tag :type="row.disabled ? 'info' : 'success'" size="small">
            {{ row.disabled ? "已停用" : "正常" }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="云同步用量" min-width="110">
        <template #default="{ row }">
          <div class="usage">
            <el-progress
              :percentage="usagePercent(row as UserAccount)"
              :stroke-width="8"
              :show-text="false"
            />
            <span class="muted small nowrap">
              {{ formatBytes(row.usedBytes) }} / {{ formatBytes(row.quotaBytes) }}
            </span>
          </div>
        </template>
      </el-table-column>
      <el-table-column prop="versions" label="版本数" width="56" align="right" />
      <el-table-column label="注册时间" width="116" align="center">
        <template #default="{ row }">
          <el-tooltip :content="formatFullDateTime(row.createdAt)" placement="top">
            <span class="nowrap">{{ formatShortDateTime(row.createdAt) }}</span>
          </el-tooltip>
        </template>
      </el-table-column>
      <el-table-column label="操作" width="226" fixed="right" align="center">
        <template #default="{ row }">
          <div class="row-actions">
            <el-button
              v-perms="'user:write'"
              link
              type="primary"
              @click="openEdit(row as UserAccount)"
            >
              配额与状态
            </el-button>
            <el-button
              v-perms="'user:write'"
              link
              type="primary"
              @click="openPassword(row as UserAccount)"
            >
              重置口令
            </el-button>
            <el-button
              v-perms="'user:write'"
              link
              type="danger"
              @click="remove(row as UserAccount)"
            >
              删除
            </el-button>
          </div>
        </template>
      </el-table-column>
      <template #empty>
        <el-empty description="还没有用户" :image-size="72" />
      </template>
    </el-table>

    <el-dialog v-model="editDialog" title="配额与状态" width="460px" destroy-on-close>
      <el-form label-position="top">
        <el-form-item label="账号">
          <el-input :model-value="editing?.username" disabled />
        </el-form-item>
        <el-form-item label="云同步配额（MB）">
          <el-input-number v-model="form.quotaMb" :min="1" :max="10240" :step="10" />
        </el-form-item>
        <el-form-item label="停用账号">
          <el-switch v-model="form.disabled" />
          <span class="muted small hint">
            停用后该用户不能登录，已有会话立即失效；数据保留。
          </span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="editDialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="save">保存</el-button>
      </template>
    </el-dialog>

    <el-dialog
      v-model="passwordDialog"
      :title="`重置 ${passwordTarget?.username ?? ''} 的口令`"
      width="440px"
      destroy-on-close
    >
      <el-form label-position="top">
        <el-form-item label="新口令">
          <el-input v-model="passwordForm.password" type="password" show-password autocomplete="new-password" />
        </el-form-item>
        <el-form-item label="再输一次">
          <el-input v-model="passwordForm.confirm" type="password" show-password autocomplete="new-password" />
        </el-form-item>
        <p class="muted small">
          邮件找回还没开放，这一步就是当前的找回通道。改完会作废该用户的所有会话。
        </p>
      </el-form>
      <template #footer>
        <el-button @click="passwordDialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="savePassword">重置口令</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.users-page {
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
  max-width: 68ch;
}
.actions {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.search {
  width: 220px;
}
.text-cell {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
}
.ellipsis {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.text-cell .el-tag {
  flex: none;
}
.row-actions {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  flex-wrap: nowrap;
  white-space: nowrap;
}
.row-actions :deep(.el-button + .el-button) {
  margin-left: 0;
}
.nowrap {
  white-space: nowrap;
}
.users-table {
  width: 100%;
}
.usage {
  display: flex;
  align-items: center;
  gap: 10px;
}
.usage :deep(.el-progress) {
  flex: 1;
}
.muted {
  color: var(--el-text-color-secondary);
}
.small {
  font-size: 12px;
}
.hint {
  margin-left: 10px;
}
</style>
