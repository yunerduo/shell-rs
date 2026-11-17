const Layout = () => import("@/layout/index.vue");

export default {
  path: "/system",
  name: "System",
  component: Layout,
  redirect: "/system/admins",
  meta: {
    icon: "ep/setting",
    title: "系统",
    rank: 3,
    roles: ["owner"]
  },
  children: [
    {
      path: "/system/users",
      name: "UserAccounts",
      component: () => import("@/views/users/index.vue"),
      meta: {
        icon: "ep/user",
        title: "用户账号",
        roles: ["owner"]
      }
    },
    {
      path: "/system/admins",
      name: "AdminAccounts",
      component: () => import("@/views/admins/index.vue"),
      meta: {
        icon: "ep/user-filled",
        title: "管理员账号",
        roles: ["owner"]
      }
    }
  ]
} satisfies RouteConfigsTable;
