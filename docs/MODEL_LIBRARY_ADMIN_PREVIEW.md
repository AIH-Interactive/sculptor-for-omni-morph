# 管理员衣柜只读预览

模型库扩展接口 `GET /api/library/v1/admin/wardrobe-preview/:kind/:id` 使用现有玩家身份、来源绑定和额外管理员会话，不改变观察者分发权限。

- `CURRENT/:owner` 必须同时传入 `asset_id`、`asset_revision`、`equipment_revision`，不匹配返回 409，避免列表刷新期间预览到替换模型。
- `BLOCK/:operation_id` 精确定位原屏蔽记录里的衣柜来源及版本，供管理员处理衣柜申诉；原资源已不存在则返回 404。
- 只返回原始压缩模型包，附 Content-Length 和 x-omnimorph-sha256，Cache-Control 为 private, no-store。每个流块再次验证管理员会话及目标来源，受现有下载并发限额和读取超时约束。
- 普通账号即使为上传者也不能调用此管理员接口。屏蔽、隐藏或私有模型的普通玩家分发规则不变。
- 不上传、不装备、不生成模型库发布记录，也不计为正式模型库下载。

实际游戏预览验收 MANUAL/PENDING。
