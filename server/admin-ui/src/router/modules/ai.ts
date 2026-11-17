const Layout = () => import("@/layout/index.vue");

export default {
  path: "/ai",
  name: "Ai",
  component: Layout,
  redirect: "/ai/settings",
  meta: {
    icon: "ep/magic-stick",
    title: "内置 AI",
    rank: 4,
    roles: ["owner"]
  },
  children: [
    {
      path: "/ai/settings",
      name: "AiTrial",
      component: () => import("@/views/ai/index.vue"),
      meta: {
        icon: "ep/magic-stick",
        title: "内置 AI · 试用",
        roles: ["owner"]
      }
    }
  ]
} satisfies RouteConfigsTable;
