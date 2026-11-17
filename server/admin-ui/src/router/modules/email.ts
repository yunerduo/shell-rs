const Layout = () => import("@/layout/index.vue");

export default {
  path: "/email",
  name: "Email",
  component: Layout,
  redirect: "/email/settings",
  meta: {
    icon: "ep/message",
    title: "邮件",
    rank: 5,
    roles: ["owner"]
  },
  children: [
    {
      path: "/email/settings",
      name: "EmailSettings",
      component: () => import("@/views/email/index.vue"),
      meta: {
        icon: "ep/message",
        title: "SMTP 与模板",
        roles: ["owner"]
      }
    }
  ]
} satisfies RouteConfigsTable;
