# Sculptor For Omni Morph

上游项目 [https://github.com/MrXiaoM/sculptor](https://github.com/MrXiaoM/sculptor)

OmniMorph 双协议后端：保留 Figura Cloud，新增 Protobuf 控制/状态及普通 HTTP 模型传输。
源码启动、Config.toml/环境变量、TLS 代理、数据迁移与回滚见 [部署说明](docs/DEPLOYMENT.md)。
直接启动使用 `cargo run --locked`，Cargo 自动完成必要的编译并读取配置，无需手动先构建 exe。
