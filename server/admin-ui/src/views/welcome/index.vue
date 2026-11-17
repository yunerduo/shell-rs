<script setup lang="ts">
defineOptions({
  name: "Welcome"
});
</script>

<template>
  <div class="welcome">
    <h1>ShellAs 后台</h1>
    <p class="muted">从左侧选择版本发布、更新记录或使用记录。</p>
    <ul>
      <li><a href="#/release/releases">版本发布</a>：上传安装包并发布清单</li>
      <li><a href="#/release/changelog">更新记录</a>：维护官网 /changelog</li>
      <li><a href="#/data/usage">使用记录</a>：查看版本、平台与功能使用</li>
    </ul>
  </div>
</template>

<style scoped>
.welcome {
  padding: 32px;
}
.welcome h1 {
  margin: 0 0 8px;
}
.muted {
  color: var(--el-text-color-secondary);
}
ul {
  margin-top: 20px;
  line-height: 2;
}
</style>
