const Layout = () => import("@/layout/index.vue");

export default {
  path: "/data",
  name: "Data",
  component: Layout,
  redirect: "/data/usage",
  meta: {
    icon: "ep/data-analysis",
    title: "数据",
    rank: 2,
    roles: ["owner", "publisher", "viewer"]
  },
  children: [
    {
      path: "/data/usage",
      name: "UsageReport",
      component: () => import("@/views/usage/index.vue"),
      meta: {
        icon: "ep/histogram",
        title: "使用记录",
        roles: ["owner", "publisher", "viewer"]
      }
    }
  ]
} satisfies RouteConfigsTable;
