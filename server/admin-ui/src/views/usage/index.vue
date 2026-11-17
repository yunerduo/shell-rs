<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import * as echarts from "echarts";
import { getUsage, type UsageOverview } from "@/api/admin";

defineOptions({ name: "UsageReport" });

const loading = ref(false);
const days = ref(30);
const data = ref<UsageOverview>({
  days: 30,
  since: "",
  totals: { events: 0, deviceDays: 0, sessions: 0 },
  daily: [],
  versions: [],
  platforms: [],
  topEvents: [],
  recent: []
});

const chartEl = ref<HTMLDivElement>();
let chart: echarts.ECharts | null = null;

function renderChart() {
  if (!chartEl.value) return;
  if (!chart) {
    chart = echarts.init(chartEl.value);
  }
  const dates = data.value.daily.map(row => row.date.slice(5));
  const events = data.value.daily.map(row => row.events);
  const devices = data.value.daily.map(row => row.deviceDays);
  chart.setOption(
    {
      grid: { left: 48, right: 24, top: 36, bottom: 36 },
      tooltip: { trigger: "axis" },
      legend: { top: 0, data: ["事件", "设备·天"] },
      xAxis: {
        type: "category",
        data: dates,
        axisTick: { show: false },
        axisLine: { lineStyle: { color: "#e4e8e9" } },
        axisLabel: { color: "#77818a" }
      },
      yAxis: {
        type: "value",
        minInterval: 1,
        splitLine: { lineStyle: { color: "#f0f3f3" } },
        axisLabel: { color: "#77818a" }
      },
      series: [
        {
          name: "事件",
          type: "bar",
          data: events,
          barMaxWidth: 26,
          itemStyle: { color: "#00a99b", borderRadius: [3, 3, 0, 0] }
        },
        {
          name: "设备·天",
          type: "line",
          data: devices,
          smooth: true,
          symbolSize: 4,
          lineStyle: { color: "#4a53b8", width: 2 },
          itemStyle: { color: "#4a53b8" }
        }
      ]
    },
    true
  );
}

function resize() {
  chart?.resize();
}

async function load() {
  loading.value = true;
  try {
    const result = await getUsage(days.value);
    data.value = result.data;
    renderChart();
  } catch (error: any) {
    ElMessage.error(error?.response?.data?.message || "读取使用记录失败");
  } finally {
    loading.value = false;
  }
}

watch(days, load);
onMounted(() => {
  load();
  window.addEventListener("resize", resize);
});
onBeforeUnmount(() => {
  window.removeEventListener("resize", resize);
  chart?.dispose();
  chart = null;
});
</script>

<template>
  <div class="usage-page" v-loading="loading">
    <div class="page-head">
      <div>
        <h2>使用记录</h2>
        <p class="muted">
          客户端匿名上报的统计。设备按天匿名，跨天认不出同一台，所以口径是「设备·天」。
        </p>
      </div>
      <el-radio-group v-model="days" size="small">
        <el-radio-button :value="7">7 天</el-radio-button>
        <el-radio-button :value="30">30 天</el-radio-button>
        <el-radio-button :value="90">90 天</el-radio-button>
      </el-radio-group>
    </div>

    <el-row :gutter="16">
      <el-col :xs="24" :sm="8">
        <el-card shadow="never" class="stat">
          <p class="kicker">事件</p>
          <h3>{{ data.totals.events.toLocaleString() }}</h3>
          <p class="muted small">最近 {{ data.days }} 天</p>
        </el-card>
      </el-col>
      <el-col :xs="24" :sm="8">
        <el-card shadow="never" class="stat">
          <p class="kicker">设备·天</p>
          <h3>{{ data.totals.deviceDays.toLocaleString() }}</h3>
          <p class="muted small">按天去重的设备</p>
        </el-card>
      </el-col>
      <el-col :xs="24" :sm="8">
        <el-card shadow="never" class="stat">
          <p class="kicker">会话</p>
          <h3>{{ data.totals.sessions.toLocaleString() }}</h3>
          <p class="muted small">客户端每次启动的会话</p>
        </el-card>
      </el-col>
    </el-row>

    <el-card shadow="never" class="block">
      <template #header>每天的事件数</template>
      <div v-show="data.daily.length" ref="chartEl" class="chart" />
      <el-empty v-if="!data.daily.length" description="这个时间段还没有收到统计事件" :image-size="80" />
    </el-card>

    <el-row :gutter="16" class="block">
      <el-col :xs="24" :md="12">
        <el-card shadow="never">
          <template #header>版本分布</template>
          <el-table :data="data.versions" size="small">
            <el-table-column prop="key" label="版本" min-width="120" />
            <el-table-column prop="deviceDays" label="设备·天" width="110" />
            <el-table-column prop="events" label="事件" width="100" />
          </el-table>
        </el-card>
      </el-col>
      <el-col :xs="24" :md="12">
        <el-card shadow="never">
          <template #header>平台分布</template>
          <el-table :data="data.platforms" size="small">
            <el-table-column prop="key" label="系统" min-width="120" />
            <el-table-column prop="deviceDays" label="设备·天" width="110" />
            <el-table-column prop="events" label="事件" width="100" />
          </el-table>
        </el-card>
      </el-col>
    </el-row>

    <el-row :gutter="16" class="block">
      <el-col :xs="24" :md="12">
        <el-card shadow="never">
          <template #header>功能使用</template>
          <el-table :data="data.topEvents" size="small">
            <el-table-column prop="key" label="事件" min-width="200" />
            <el-table-column prop="events" label="次数" width="100" />
          </el-table>
        </el-card>
      </el-col>
      <el-col :xs="24" :md="12">
        <el-card shadow="never">
          <template #header>最近事件</template>
          <el-table :data="data.recent" size="small">
            <el-table-column prop="occurredAt" label="时间" width="170" />
            <el-table-column prop="name" label="事件" min-width="150" />
            <el-table-column prop="appVersion" label="版本" width="100" />
            <el-table-column prop="osName" label="系统" width="110" />
            <el-table-column label="构建" width="80">
              <template #default="{ row }">
                <el-tag :type="row.isDebug ? 'warning' : 'success'" size="small">
                  {{ row.isDebug ? "调试" : "正式" }}
                </el-tag>
              </template>
            </el-table-column>
          </el-table>
        </el-card>
      </el-col>
    </el-row>
  </div>
</template>

<style scoped>
.usage-page {
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
.stat {
  margin-bottom: 16px;
}
.stat h3 {
  margin: 4px 0;
  font-size: 26px;
}
.stat p:last-child {
  margin: 0;
}
.kicker {
  margin: 0;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--el-text-color-secondary);
}
.block {
  margin-top: 18px;
}
.chart {
  width: 100%;
  height: 320px;
}
</style>
