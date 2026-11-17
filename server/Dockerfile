# 后台界面：Vue 3 + Element Plus（pure-admin-thin），只在这里构建成静态文件。
FROM node:22-bookworm-slim AS ui
RUN npm install -g pnpm@11
WORKDIR /ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/pnpm-workspace.yaml admin-ui/.npmrc ./
RUN pnpm install --frozen-lockfile
COPY admin-ui/ ./
RUN pnpm build

# 构建阶段：独立项目，只需要 server/ 自己的源码。
# 工具链与仓库的 rust-toolchain.toml 一致；镜像不存在时换成 rust:1-bookworm。
FROM rust:1.98-bookworm AS build
# rustls 的 aws-lc-sys 用 CMake 构建。
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# 先把依赖编一遍，之后改业务代码不必重下依赖。
RUN mkdir src && printf 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
COPY web ./web
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data shellas
COPY --from=build /src/target/release/shellas-server /usr/local/bin/shellas-server
COPY --from=ui /ui/dist /app/admin-ui
WORKDIR /data
USER shellas
ENV SHELLAS_BIND=0.0.0.0:8080 \
    SHELLAS_DATA_DIR=/data \
    SHELLAS_ADMIN_UI_DIR=/app/admin-ui
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["shellas-server", "serve"]
