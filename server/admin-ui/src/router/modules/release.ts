const Layout = () => import("@/layout/index.vue");

export default {
  path: "/release",
  name: "Release",
  component: Layout,
  redirect: "/release/releases",
  meta: {
    icon: "ep/promotion",
    title: "版本",
    rank: 1,
    roles: ["owner", "publisher"]
  },
  children: [
    {
      path: "/release/releases",
      name: "ReleaseManage",
      component: () => import("@/views/releases/index.vue"),
      meta: {
        icon: "ep/upload-filled",
        title: "版本发布",
        roles: ["owner", "publisher"]
      }
    },
    {
      path: "/release/changelog",
      name: "ChangelogManage",
      component: () => import("@/views/changelog/index.vue"),
      meta: {
        icon: "ep/document",
        title: "更新记录",
        roles: ["owner", "publisher"]
      }
    }
  ]
} satisfies RouteConfigsTable;
