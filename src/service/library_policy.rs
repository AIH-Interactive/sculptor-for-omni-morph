//! Shared policy decisions; handlers must not infer access from list membership.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Moderation {
    Active,
    Blocked,
    Deleted,
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub uploader: Uuid,
    pub visibility: Visibility,
    pub requires_authorization: bool,
    pub moderation: Moderation,
}

#[derive(Clone, Copy, Debug)]
pub struct Actor {
    pub uuid: Uuid,
    /// True only after validating the additional administrator session.
    pub admin_context: bool,
    pub granted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permissions {
    pub visible: bool,
    pub use_model: bool,
    pub manage: bool,
}

impl Policy {
    pub fn permissions(self, actor: Actor) -> Permissions {
        let owner = actor.uuid == self.uploader;
        let visible = if actor.admin_context {
            true
        } else {
            self.moderation == Moderation::Active
                && (owner || self.visibility == Visibility::Public)
        };
        let use_model = visible
            && self.moderation != Moderation::Deleted
            && (actor.admin_context
                || owner
                || (self.visibility == Visibility::Public
                    && (!self.requires_authorization || actor.granted)));
        Permissions {
            visible,
            use_model,
            manage: visible && (owner || actor.admin_context),
        }
    }

    /// Distribution is evaluated as the observer, never with the wearer's grants.
    pub fn can_distribute(self, wearer: Uuid, observer: Uuid, granted: bool) -> bool {
        self.can_render_equipment(wearer, observer, granted, false)
    }

    /// This exception belongs to the current source-bound equipment, not catalog access.
    pub fn can_render_equipment(
        self,
        wearer: Uuid,
        observer: Uuid,
        granted: bool,
        show_private_equipment: bool,
    ) -> bool {
        if self.moderation != Moderation::Active {
            return false;
        }
        if self.visibility == Visibility::Private {
            return wearer == self.uploader && (wearer == observer || show_private_equipment);
        }
        self.permissions(Actor {
            uuid: observer,
            admin_context: false,
            granted,
        })
        .use_model
            && (self.visibility == Visibility::Public || wearer == observer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_visibility_authorization_and_moderation_matrix() {
        let owner = Uuid::from_u128(1);
        for visibility in [Visibility::Public, Visibility::Private] {
            for requires_authorization in [false, true] {
                for moderation in [Moderation::Active, Moderation::Blocked, Moderation::Deleted] {
                    for (uuid, granted, admin_context) in [
                        (owner, false, false),
                        (Uuid::from_u128(2), true, false),
                        (Uuid::from_u128(3), false, false),
                        (Uuid::from_u128(4), false, true),
                    ] {
                        let policy = Policy {
                            uploader: owner,
                            visibility,
                            requires_authorization,
                            moderation,
                        };
                        let permissions = policy.permissions(Actor {
                            uuid,
                            granted,
                            admin_context,
                        });
                        let expected_visible = admin_context
                            || (moderation == Moderation::Active
                                && (uuid == owner || visibility == Visibility::Public));
                        let expected_use = moderation != Moderation::Deleted
                            && (admin_context
                                || (moderation == Moderation::Active
                                    && (uuid == owner
                                        || (visibility == Visibility::Public
                                            && (!requires_authorization || granted)))));
                        assert_eq!(permissions.visible, expected_visible);
                        assert_eq!(permissions.use_model, expected_use);
                        assert_eq!(
                            permissions.manage,
                            expected_visible && (admin_context || uuid == owner)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn observers_cannot_inherit_wearer_or_administrator_permissions() {
        let owner = Uuid::from_u128(1);
        let observer = Uuid::from_u128(2);
        let mut policy = Policy {
            uploader: owner,
            visibility: Visibility::Private,
            requires_authorization: true,
            moderation: Moderation::Active,
        };
        assert!(!policy.can_distribute(owner, observer, true));
        assert!(policy.can_distribute(owner, owner, false));
        policy.visibility = Visibility::Public;
        assert!(!policy.can_distribute(owner, observer, false));
        assert!(policy.can_distribute(owner, observer, true));
        policy.moderation = Moderation::Blocked;
        assert!(!policy.can_distribute(owner, owner, true));
        // Blocked wardrobe copies are separately SELF_ONLY, not library downloads.
    }

    #[test]
    fn private_equipment_opt_in_changes_rendering_without_opening_catalog_access() {
        let owner = Uuid::from_u128(1);
        let observer = Uuid::from_u128(2);
        for requires_authorization in [false, true] {
            let mut policy = Policy {
                uploader: owner,
                visibility: Visibility::Private,
                requires_authorization,
                moderation: Moderation::Active,
            };
            assert!(!policy.can_render_equipment(owner, observer, true, false));
            assert!(policy.can_render_equipment(owner, observer, false, true));
            assert!(!policy.can_render_equipment(observer, owner, true, true));
            assert!(policy.can_render_equipment(owner, owner, false, false));
            let access = policy.permissions(Actor {
                uuid: observer,
                admin_context: false,
                granted: true,
            });
            assert!(!access.visible && !access.use_model);
            for moderation in [Moderation::Blocked, Moderation::Deleted] {
                policy.moderation = moderation;
                assert!(!policy.can_render_equipment(owner, observer, true, true));
            }
        }
    }
}
