# 模型库与衣柜存储 / 回收站

对应客户端计划 R1，版本仍为 alpha.2。

分类内置为 `full_model`（完整模型）及 `decorative_parts`（装饰部件）。配置文件默认 `categories = []`，仅可添加额外分类，不能移除、替换或重复声明内置分类。

默认路径相对于后端工作目录；avatars 路径可由原环境变量覆盖：

- `data/avatars/models/`：显式上传到模型库的模型包及自定义封面，SHA-256 文件名。
- `data/avatars/wardrobe/`：衣柜上传；新版资源按 SHA-256 命名，旧 Figura 上传为 `<UUID>.moon`。
- `data/avatars/upload/`：上传中的临时文件。
- `data/avatars/recycle/library/<模型UUID>/`：删除的模型库文件、封面及原状态快照。
- `data/avatars/recycle/wardrobe/<玩家UUID>/<删除操作UUID>/`：衣柜删除的文件、各版本 AvatarDescriptor protobuf 和 metadata.json。衣柜记录供管理员手工恢复，不新增衣柜回收站页面。
- `data/metadata.sqlite3`：模型库与玩家资源、权限、恢复状态等记录；备份和恢复需与文件目录一起处理。

启动时将原 models 目录中的衣柜文件按数据库来源分离；同内容确实同时用于模型库和衣柜时保留两个副本。模型库列表始终读取发布记录，不扫描衣柜目录。升级至 SQLite schema 5 前沿用原备份机制；旧已删除模型补建回收记录，屏蔽状态按既有哈希屏蔽记录保守恢复。较旧后端不支持打开 schema 5，不应直接降级运行。

普通删除先保存可恢复文件，再提交不可见状态，再清理无活跃共享引用的源文件。发布、删除及恢复共用文件变更锁。共享模型不会因其中一项删除而丢失。回收站不自动清空。

管理员接口沿用玩家认证、来源绑定及额外管理员会话：

- `POST /api/library/v1/admin/models/:id/restore`
- `POST /api/library/v1/admin/models/:id/purge`

请求均为 `{ "operation_id": "UUID", "expected_revision": <policy_revision> }`，保持 CAS 和操作幂等性。只允许处理回收站中的对象。恢复保留原状态及现有屏蔽约束，不自动公开或装备。彻底删除清除该对象的回收文件，保留必要的审计、屏蔽及不可恢复标记；其他模型或衣柜仍引用的文件不受影响。旧恢复操作重试不能删除新一轮删除的回收文件。

自动检查增加真实 HTTP/生产路径测试，覆盖衣柜不入目录、同哈希双来源隔离、归档内容、权限/CAS、恢复、彻底删除、共享引用、屏蔽恢复、恢复旧操作重放和旧删除记录重启归档。原迁移测试更新为衣柜独立路径。完整 cargo test 和 cargo build 在前台通过 Tee 日志运行并核对退出码。跨语言/旧进程外部夹具未配置时仍按原测试明确 SKIPPED；worker saturation 仍按原策略单独运行。

真实部署迁移、玩家游戏内上传/预览/删除/恢复效果：MANUAL/PENDING。

私有装备展示设置：GET /api/library/v1/session 现在额外返回 equipment_model_id、equipment_model_revision、show_private_equipment，均只来自当前认证玩家的装备。屏蔽或非私有来源回读为 false。客户端沿用原 equip CAS 接口保存当前来源的展示意图，无需重新下载或替换本地模型。回归先证明旧 session 缺少状态，再验证保存后回读和其他账号隔离；完整 cargo test 为 94 passed、0 failed、1 ignored，cargo build 通过。
