import { http } from "@/utils/http";
import { getToken } from "@/utils/auth";

export interface ApiResult<T> {
  success: boolean;
  data: T;
}

export interface ChannelInfo {
  version: string;
  severity: string;
  notes: string;
  notesUrl: string;
  publishedAt: string;
}

export interface ReleaseFile {
  name: string;
  platform: string;
  arch: string;
  size: number;
  sha256: string;
  downloads: number;
  uploadedAt: string;
  withdrawn: boolean;
}

export interface ReleaseVersion {
  version: string;
  files: ReleaseFile[];
  totalSize: number;
  downloads: number;
}

export interface PublishedManifest {
  channel: string;
  version: string;
  severity: string;
  publishedAt: string;
  withdrawn: boolean;
}

export interface ReleaseRecord {
  version: string;
  channel: string;
  severity: string;
  notes: string;
  notesUrl: string;
  minimumVersion: string | null;
  rollout: number;
  publishedAt: string;
  withdrawn: boolean;
  packageCount: number;
  totalSize: number;
  downloads: number;
}

export interface ReleaseHash {
  name: string;
  platform: string;
  arch: string;
  size: number;
  sha256: string;
}

export interface ReleaseOverview {
  stable: ChannelInfo | null;
  beta: ChannelInfo | null;
  manifests: PublishedManifest[];
  releases: ReleaseVersion[];
  records: ReleaseRecord[];
}

export interface UserAccount {
  id: number;
  username: string;
  email: string | null;
  phone: string | null;
  phoneVerified: boolean;
  emailVerified: boolean;
  quotaBytes: number;
  usedBytes: number;
  versions: number;
  createdAt: string;
  disabled: boolean;
}

export interface PackageSpec {
  platform: string;
  file: string;
  asset: boolean;
  installer: boolean;
}

export interface PublishPayload {
  channel: string;
  version: string;
  severity: string;
  notes: string;
  notesUrl: string;
  minimumVersion: string;
  rollout: number;
}

export interface ChangelogEntry {
  id: number;
  version: string;
  channel: string;
  severity: string;
  title: string;
  body: string;
  publishedAt: string;
  updatedAt: string;
}

export interface ChangelogPayload {
  version: string;
  channel: string;
  severity: string;
  title: string;
  body: string;
  publishedAt: string;
}

export interface UsageRow {
  key: string;
  deviceDays: number;
  events: number;
}

export interface DailyUsage {
  date: string;
  deviceDays: number;
  events: number;
}

export interface RecentUsage {
  name: string;
  appVersion: string;
  osName: string;
  occurredAt: string;
  isDebug: boolean;
}

export interface UsageOverview {
  days: number;
  since: string;
  totals: { events: number; deviceDays: number; sessions: number };
  daily: DailyUsage[];
  versions: UsageRow[];
  platforms: UsageRow[];
  topEvents: UsageRow[];
  recent: RecentUsage[];
}

export interface AdminAccount {
  id: number;
  username: string;
  role: string;
  createdAt: string;
}

export interface AiSettings {
  enabled: boolean;
  baseUrl: string;
  model: string;
  hasApiKey: boolean;
  maxContextTokens: number;
  maxOutputTokens: number;
  temperature: number | null;
  timeoutSeconds: number;
  retryCount: number;
  dailyRequestLimit: number;
  dailyTokenLimit: number;
  maxConcurrentPerUser: number;
  maxRequestBytes: number;
  updatedAt: string;
}

export interface AiSettingsPayload {
  enabled?: boolean;
  baseUrl?: string;
  apiKey?: string;
  clearApiKey?: boolean;
  model?: string;
  maxContextTokens?: number;
  maxOutputTokens?: number;
  temperature?: number;
  clearTemperature?: boolean;
  timeoutSeconds?: number;
  retryCount?: number;
  dailyRequestLimit?: number;
  dailyTokenLimit?: number;
  maxConcurrentPerUser?: number;
  maxRequestBytes?: number;
}

export interface AiTestResult {
  ok: boolean;
  category:
    | "ok"
    | "unreachable"
    | "timeout"
    | "auth_failed"
    | "rejected"
    | "upstream_error"
    | "not_configured";
  detail: string;
  latencyMs: number;
  checkedAt: string;
}

export interface AiUsageRow {
  userId: number;
  username: string;
  accountDisabled: boolean;
  aiEnabled: boolean;
  requestsToday: number;
  tokensToday: number;
  totalRequests: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  lastUsedAt: string | null;
}

export interface AiUsagePage {
  total: number;
  items: AiUsageRow[];
}

export interface EmailSettings {
  enabled: boolean;
  host: string;
  port: number;
  encryption: "starttls" | "tls" | "plain";
  username: string;
  hasPassword: boolean;
  fromAddress: string;
  fromName: string;
  requireVerification: boolean;
  configured: boolean;
  warning: string | null;
  updatedAt: string;
}

export interface EmailSettingsPayload {
  enabled?: boolean;
  host?: string;
  port?: number;
  encryption?: "starttls" | "tls" | "plain";
  username?: string;
  password?: string;
  clearPassword?: boolean;
  fromAddress?: string;
  fromName?: string;
  requireVerification?: boolean;
}

export interface EmailTestResult {
  ok: boolean;
  category:
    | "ok"
    | "unreachable"
    | "auth_failed"
    | "rejected"
    | "not_configured";
  detail: string;
  to: string;
  latencyMs: number;
  sentAt: string;
}

export interface EmailTemplate {
  purpose: "register_verification" | "password_reset";
  subject: string;
  body: string;
  placeholders: string[];
  configured: boolean;
  updatedAt: string;
}

export interface EmailPreview {
  subject: string;
  body: string;
  variables: Record<string, string>;
}

export interface EmailSendRow {
  id: number;
  email: string;
  purpose: "register_verification" | "password_reset" | "test";
  result: "ok" | "unreachable" | "auth_failed" | "rejected" | "not_configured";
  detail: string;
  createdAt: string;
}

export interface EmailSendPage {
  total: number;
  items: EmailSendRow[];
}

export const getReleaseOverview = () =>
  http.request<ApiResult<ReleaseOverview>>("get", "/releases");

export const getPackageSpecs = (version: string) =>
  http.request<ApiResult<PackageSpec[]>>("get", "/releases/packages", {
    params: { version }
  });

export const publishRelease = (data: PublishPayload) =>
  http.request<ApiResult<unknown>>("post", "/release", { data });

export const getReleaseHashes = (version: string) =>
  http.request<ApiResult<ReleaseHash[]>>(
    "get",
    `/releases/${encodeURIComponent(version)}/hashes`
  );

export const withdrawRelease = (version: string, channel: string) =>
  http.request<ApiResult<unknown>>(
    "post",
    `/releases/${encodeURIComponent(version)}/withdraw`,
    { data: { channel } }
  );

export const republishRelease = (version: string, channel: string) =>
  http.request<ApiResult<unknown>>(
    "post",
    `/releases/${encodeURIComponent(version)}/republish`,
    { data: { channel } }
  );

export const getUsers = () =>
  http.request<ApiResult<UserAccount[]>>("get", "/users");

export const updateUser = (
  id: number,
  data: { quotaMb?: number; disabled?: boolean }
) => http.request<ApiResult<UserAccount>>("put", `/users/${id}`, { data });

export const resetUserPassword = (id: number, password: string) =>
  http.request<ApiResult<unknown>>("post", `/users/${id}/password`, {
    data: { password }
  });

export const deleteUser = (id: number) =>
  http.request<ApiResult<unknown>>("delete", `/users/${id}`);

export const getChangelog = () =>
  http.request<ApiResult<ChangelogEntry[]>>("get", "/changelog");

export const createChangelog = (data: ChangelogPayload) =>
  http.request<ApiResult<ChangelogEntry>>("post", "/changelog", { data });

export const updateChangelog = (id: number, data: ChangelogPayload) =>
  http.request<ApiResult<ChangelogEntry>>("put", `/changelog/${id}`, { data });

export const deleteChangelog = (id: number) =>
  http.request<ApiResult<unknown>>("delete", `/changelog/${id}`);

export const getUsage = (days: number) =>
  http.request<ApiResult<UsageOverview>>("get", "/usage", {
    params: { days }
  });

export const getAdmins = () =>
  http.request<ApiResult<AdminAccount[]>>("get", "/admins");

export const createAdmin = (data: {
  username: string;
  password: string;
  role: string;
}) => http.request<ApiResult<AdminAccount>>("post", "/admins", { data });

export const updateAdmin = (
  id: number,
  data: { role?: string; password?: string }
) => http.request<ApiResult<AdminAccount>>("put", `/admins/${id}`, { data });

export const deleteAdmin = (id: number) =>
  http.request<ApiResult<unknown>>("delete", `/admins/${id}`);

export const changePassword = (data: {
  currentPassword: string;
  newPassword: string;
}) => http.request<ApiResult<unknown>>("post", "/account/password", { data });

export const getAiSettings = () =>
  http.request<ApiResult<AiSettings>>("get", "/ai/settings");

export const saveAiSettings = (data: AiSettingsPayload) =>
  http.request<ApiResult<AiSettings>>("put", "/ai/settings", { data });

export const testAiUpstream = () =>
  http.request<ApiResult<AiTestResult>>("post", "/ai/test");

export const getAiUsage = (params: {
  page?: number;
  pageSize?: number;
  search?: string;
}) => http.request<ApiResult<AiUsagePage>>("get", "/ai/usage", { params });

export const setAiUserEnabled = (id: number, enabled: boolean) =>
  http.request<ApiResult<unknown>>("put", `/ai/users/${id}`, {
    data: { enabled }
  });

export const getEmailSettings = () =>
  http.request<ApiResult<EmailSettings>>("get", "/email/settings");

export const saveEmailSettings = (data: EmailSettingsPayload) =>
  http.request<ApiResult<EmailSettings>>("put", "/email/settings", { data });

export const sendEmailTest = (to: string) =>
  http.request<ApiResult<EmailTestResult>>("post", "/email/test", {
    data: { to }
  });

export const getEmailTemplates = () =>
  http.request<ApiResult<EmailTemplate[]>>("get", "/email/templates");

export const saveEmailTemplate = (
  purpose: EmailTemplate["purpose"],
  data: { subject: string; body: string }
) =>
  http.request<ApiResult<EmailTemplate>>(
    "put",
    `/email/templates/${purpose}`,
    { data }
  );

export const previewEmailTemplate = (
  purpose: EmailTemplate["purpose"],
  data: { subject?: string; body?: string }
) =>
  http.request<ApiResult<EmailPreview>>(
    "post",
    `/email/templates/${purpose}/preview`,
    { data }
  );

export const getEmailSends = (params: { limit?: number; offset?: number }) =>
  http.request<ApiResult<EmailSendPage>>("get", "/email/sends", { params });

/**
 * 上传安装包。走 XHR 是为了拿到上传进度；令牌与 axios 用同一个。
 * 失败时优先读后端 `{message}`。
 */
export function uploadPackage(
  version: string,
  fileName: string,
  file: File,
  onProgress?: (percent: number) => void
): Promise<{ size: number; sha256: string }> {
  const token = getToken()?.accessToken ?? "";
  const url = `/admin/api/releases/${encodeURIComponent(version)}/${encodeURIComponent(fileName)}`;
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", url);
    xhr.setRequestHeader("Authorization", `Bearer ${token}`);
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.upload.onprogress = event => {
      if (event.lengthComputable && onProgress) {
        onProgress(Math.round((event.loaded / event.total) * 100));
      }
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          const data = JSON.parse(xhr.responseText);
          resolve({ size: Number(data.size) || file.size, sha256: data.sha256 || "" });
        } catch {
          resolve({ size: file.size, sha256: "" });
        }
      } else {
        reject(new Error(readMessage(xhr) || `上传失败（HTTP ${xhr.status}）`));
      }
    };
    xhr.onerror = () => reject(new Error("上传失败：网络中断"));
    xhr.send(file);
  });
}

function readMessage(xhr: XMLHttpRequest): string {
  try {
    const data = JSON.parse(xhr.responseText);
    return data?.message || data?.error || "";
  } catch {
    return xhr.responseText?.slice(0, 200) ?? "";
  }
}
