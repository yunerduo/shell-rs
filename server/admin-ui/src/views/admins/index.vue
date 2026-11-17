<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { useUserStoreHook } from "@/store/modules/user";
import {
  changePassword,
  createAdmin,
  deleteAdmin,
  getAdmins,
  updateAdmin,
  type AdminAccount
} from "@/api/admin";
import { formatFullDateTime, formatShortDateTime } from "@/utils/datetime";

defineOptions({ name: "AdminAccounts" });

const loading = ref(false);
const saving = ref(false);
const rows = ref<AdminAccount[]>([]);
const dialog = ref(false);
const editing = ref<AdminAccount | null>(null);
const passwordDialog = ref(false);
const userStore = useUserStoreHook();

const form = reactive({
  username: "",
  password: "",
  role: "publisher"
});

const passwordForm = reactive({
  currentPassword: "",
  newPassword: "",
  confirmPassword: ""
});

const roles = [
  { value: "owner", label: "owner（全权）" },
  { value: "publisher", label: "publisher（发布与更新记录）" },
  { value: "viewer", label: "viewer（只读）" }
];

function roleLabel(role: string) {
  return roles.find(item => item.value === role)?.label ?? role;
}

function roleType(role: string) {
  if (role === "owner") return "danger";
  if (role === "publisher") return "warning";
  return "info";
}

async function load() {
  loading.value = true;
  try {
    const result = await getAdmins();
    rows.value = result.data;
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "读取管理员失败");
  } finally {
    loading.value = false;
  }
}

function openCreate() {
  editing.value = null;
  form.username = "";
  form.password = "";
  form.role = "publisher";
  dialog.value = true;
}

function openEdit(row: AdminAccount) {
  editing.value = row;
  form.username = row.username;
  form.password = "";
  form.role = row.role;
  dialog.value = true;
}

async function save() {
  saving.value = true;
  try {
    if (editing.value) {
      const payload: { role?: string; password?: string } = { role: form.role };
      if (form.password) payload.password = form.password;
      await updateAdmin(editing.value.id, payload);
      ElMessage.success("已保存");
    } else {
      await createAdmin({
        username: form.username.trim(),
        password: form.password,
        role: form.role
      });
      ElMessage.success("已创建管理员");
    }
    dialog.value = false;
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function remove(row: AdminAccount) {
  try {
    await ElMessageBox.confirm(`删除管理员 ${row.username}？`, "删除管理员", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消"
    });
  } catch {
    return;
  }
  try {
    await deleteAdmin(row.id);
    ElMessage.success("已删除");
    await load();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "删除失败");
  }
}

function openPassword() {
  passwordForm.currentPassword = "";
  passwordForm.newPassword = "";
  passwordForm.confirmPassword = "";
  passwordDialog.value = true;
}

async function savePassword() {
  if (passwordForm.newPassword !== passwordForm.confirmPassword) {
    ElMessage.warning("两次输入的新口令不一致");
    return;
  }
  saving.value = true;
  try {
    await changePassword({
      currentPassword: passwordForm.currentPassword,
      newPassword: passwordForm.newPassword
    });
    passwordDialog.value = false;
    ElMessage.success("口令已更新，请重新登录");
    userStore.logOut();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "修改口令失败");
  } finally {
    saving.value = false;
  }
}

onMounted(load);
</script>

<template>
  <div class="admins-page">
    <div class="page-head">
      <div>
        <h2>管理员账号</h2>
        <p class="muted">
          后台账号与客户端账号分开。owner 管全部，publisher 管发布与更新记录，viewer 只看。
        </p>
      </div>
      <div class="actions">
        <el-button @click="openPassword">修改我的口令</el-button>
        <el-button v-perms="'admin:write'" type="primary" @click="openCreate">
          新增管理员
        </el-button>
      </div>
    </div>

    <el-table v-loading="loading" :data="rows" size="small">
      <el-table-column prop="username" label="账号" min-width="180">
        <template #default="{ row }">
          {{ row.username }}
          <el-tag v-if="row.username === userStore.username" size="small" type="success">
            当前登录
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="角色" min-width="240">
        <template #default="{ row }">
          <el-tag :type="roleType(row.role)" size="small">{{ roleLabel(row.role) }}</el-tag>
        </template>
      </el-table-column>
      <el-table-column label="创建时间" width="128" align="center">
        <template #default="{ row }">
          <el-tooltip :content="formatFullDateTime(row.createdAt)" placement="top">
            <span>{{ formatShortDateTime(row.createdAt) }}</span>
          </el-tooltip>
        </template>
      </el-table-column>
      <el-table-column label="操作" width="170" fixed="right">
        <template #default="{ row }">
          <el-button v-perms="'admin:write'" link type="primary" @click="openEdit(row as AdminAccount)">
            编辑
          </el-button>
          <el-button
            v-perms="'admin:write'"
            link
            type="danger"
            :disabled="row.username === userStore.username"
            @click="remove(row as AdminAccount)"
          >
            删除
          </el-button>
        </template>
      </el-table-column>
    </el-table>

    <el-dialog
      v-model="dialog"
      :title="editing ? `编辑 ${editing.username}` : '新增管理员'"
      width="480px"
      destroy-on-close
    >
      <el-form label-position="top">
        <el-form-item label="账号">
          <el-input v-model="form.username" :disabled="!!editing" placeholder="3–32 个字符" />
        </el-form-item>
        <el-form-item :label="editing ? '重置口令（留空不改）' : '口令'">
          <el-input v-model="form.password" type="password" show-password autocomplete="new-password" />
        </el-form-item>
        <el-form-item label="角色">
          <el-select v-model="form.role" class="w-full">
            <el-option v-for="item in roles" :key="item.value" :label="item.label" :value="item.value" />
          </el-select>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="save">保存</el-button>
      </template>
    </el-dialog>

    <el-dialog v-model="passwordDialog" title="修改我的口令" width="460px" destroy-on-close>
      <el-form label-position="top">
        <el-form-item label="当前口令">
          <el-input v-model="passwordForm.currentPassword" type="password" show-password />
        </el-form-item>
        <el-form-item label="新口令">
          <el-input v-model="passwordForm.newPassword" type="password" show-password />
        </el-form-item>
        <el-form-item label="再输一次新口令">
          <el-input v-model="passwordForm.confirmPassword" type="password" show-password />
        </el-form-item>
        <p class="muted small">改完口令后所有后台会话都会失效，需要重新登录。</p>
      </el-form>
      <template #footer>
        <el-button @click="passwordDialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="savePassword">更新口令</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.admins-page {
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
.actions {
  display: flex;
  gap: 10px;
}
.muted {
  color: var(--el-text-color-secondary);
}
.small {
  font-size: 12px;
}
.w-full {
  width: 100%;
}
</style>
