# 统一上传与 SQLite 元数据

alpha.2 模型库当前使用 `user_version=4`：在既有账号/装备/模型库数据基础上追加举报、申诉、
衣柜来源屏蔽及资源版本展示设置。schema 3→4 在同一事务迁移，迁移前使用 SQLite backup
生成 `metadata.pre-library-<随机值>.db`；不重写头像本体，不把私有装备展示默认值与衣柜默认值混用。
旧版本不能直接打开新 schema；回退须配套恢复数据库与资源备份。合成库的迁移/重复打开、
旧策略保留、未知版本拒绝与实际新旧进程保留检查已通过，生产恢复演练仍为 MANUAL/PENDING。

下文保留最初账号持久化修复的背景与当时检查记录；最新模型库证据见
[MODEL_LIBRARY_API.md](MODEL_LIBRARY_API.md) 与客户端模型库实施计划。

本次修复的回归场景：已有模型上传并保留在磁盘，后端重启后注册用户表丢失，
观察者按玩家 UUID 刷新资料得到 NOT_FOUND。注册资料原来只保存在 UManager 内存中。

## 存储约定

- `data/avatars/upload/<uuid>-<operation>.part`：所有引擎和上传入口共用的暂存区。
  接收完成后执行大小、包类型和哈希校验；失败或取消删除暂存，未完成上传重启后不能提交。
- `data/avatars/models/<sha256>`：不可变模型本体，继续使用文件存储。
- `data/metadata.db`：唯一持久业务元数据存储，WAL、FULL synchronous、外键检查、事务提交。
- `users` / `upload_permissions`：UUID 资料、封禁、上传权限；真实认证令牌不持久化。
- `owners` / `avatars` / `equipment`：UUID、模型 ID、revision、引擎类型、分发格式、内容摘要、
  文件长度、删除状态及主装备/兼容装备关联。引擎类型存数值，不按引擎复制存储流程。
- `operations` / `ysm_state` / `metadata`：幂等结果、恢复状态和稳定 instance ID。

配置文件仍是运营配置输入。会话、认证挑战、订阅、未完成上传授权和队列是临时连接状态，
重启后要求重新认证，不能由持久模型关联授予登录权限。

## 迁移

默认目录为 `data/metadata.db`、`data/avatars/upload`、`data/avatars/models`。
`AVATARS_FOLDER` 只改变 avatars 根目录，数据库仍相对于工作目录位于 `data/metadata.db`。
首次升级到 `.db` 时，若目标不存在，优先从同目录的 `metadata.sqlite3` 使用 SQLite backup
导入一致快照（包括已提交 WAL）；其次兼容旧 `cloud-v1/metadata.db` 或 `cloud-v1/metadata.sqlite3`。
原库保留用于恢复；后续仅使用已存在的 `.db`，不会再被旧库覆盖。未知版本拒绝迁移，避免创建空库。
停止旧服务后启动新版本，会复制旧 `cloud-v1/blobs`、根目录 UUID `.moon` 和 `temp` 中的模型；
现有目标文件优先，原文件保留。旧数据库使用 SQLite backup 接口复制，包含已提交的 WAL 内容。
文件复制和数据库打开成功后在 SQLite 记录迁移完成，后续启动不会重新导入已消费的临时文件。
`cloud-v1/uploads/*.part` 是无有效授权的未完成上传，不迁移。

首次打开自动导入旧 `manifest.json` schema 1，装备、历史 revision、tombstone、幂等结果和
YSM 恢复数据与 instance ID 保持。源文件不修改；SQLite 初始化标记和导入数据同事务提交。
后续启动只读取 SQLite。未知数据库版本或未知旧 manifest 版本拒绝启动，不清理 upload。
历史 `.moon` 保留原字节并继续按已有惰性导入流程补建索引；旧模型 UUID 可以恢复资料，
无需模型主人重新登录，观察者即可按 UUID 获取已装备模型。

上线前停止旧服务并备份整个 avatars 目录；本次开发没有操作正在部署的数据目录或重启生产服务。
备份与回滚注意事项见 [DEPLOYMENT.md](DEPLOYMENT.md)。

## 自动检查

- 修复前 `cargo test registered_profiles_survive_restart_with_model_associations -- --nocapture`
  失败：重启后同 UUID 的模型可读，但资料不存在。
- 修复后同一回归通过，并检查上传权限/封禁保留、旧会话失效及 UUID 模型关联。
- 旧 manifest 单次迁移、直接 SQL 查询 UUID/模型、外键完整性、SQLite 注入提交失败、
  真实子进程 COMMIT 前终止、重启、tombstone 与未知版本拒绝均通过。
- 全部 25 项 Rust 测试通过，包含双协议本地 HTTP/WS 生命周期、观察者重启查询与账号提交失败保护。
  `cargo build --locked --release` 成功，Windows 可执行文件位于 `target/release/sculptor.exe`。
  未设置旧生产二进制/Java classpath 时，相应可选跨版本/跨语言门禁不作为已运行。

实机游戏刷新与上传进度界面按用户要求交人工验收。
