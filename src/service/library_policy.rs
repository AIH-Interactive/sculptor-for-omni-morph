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

    /// Distribution of worn equipment follows the world display rule of `can_render_equipment`.
    pub fn can_distribute(self, wearer: Uuid, observer: Uuid, wearer_granted: bool) -> bool {
        self.can_render_equipment(wearer, observer, wearer_granted, false)
    }

    /// Whether observers see the avatar a wearer currently uses in the game world. This belongs to the
    /// current source-bound equipment, not catalog access: it is decided by the wearer's right to use
    /// the model (`wearer_granted` is the wearer's grant), never by the observer's own grant. Private
    /// models are only worn by their uploader and follow the wearer's display setting.
    pub fn can_render_equipment(
        self,
        wearer: Uuid,
        observer: Uuid,
        wearer_granted: bool,
        show_private_equipment: bool,
    ) -> bool {
        if self.moderation != Moderation::Active {
            return false;
        }
        if self.visibility == Visibility::Private {
            return wearer == self.uploader && (wearer == observer || show_private_equipment);
        }
        self.permissions(Actor {
            uuid: wearer,
            admin_context: false,
            granted: wearer_granted,
        })
        .use_model
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
        // The observer's own grant is irrelevant: the uploader may always wear the model.
        assert!(policy.can_distribute(owner, observer, false));
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
    #[test]
    fn world_display_follows_the_wearer_authorization_not_the_observer() {
        let owner = Uuid::from_u128(1);
        let wearer = Uuid::from_u128(2);
        let observer = Uuid::from_u128(3);
        let mut policy = Policy {
            uploader: owner,
            visibility: Visibility::Public,
            requires_authorization: true,
            moderation: Moderation::Active,
        };
        // An authorized wearer is seen by observers without a grant of their own.
        assert!(policy.can_render_equipment(wearer, observer, true, false));
        // A wearer without (or after losing) the grant is not displayed.
        assert!(!policy.can_render_equipment(wearer, observer, false, false));
        policy.requires_authorization = false;
        assert!(policy.can_render_equipment(wearer, observer, false, false));
        // Private models are only worn by their uploader and shown when the display setting allows it.
        policy.visibility = Visibility::Private;
        assert!(!policy.can_render_equipment(wearer, observer, true, true));
        assert!(policy.can_render_equipment(owner, observer, false, true));
        assert!(!policy.can_render_equipment(owner, observer, false, false));
        // Blocked models stay hidden whatever the display rule.
        policy.visibility = Visibility::Public;
        policy.moderation = Moderation::Blocked;
        assert!(!policy.can_render_equipment(owner, observer, true, true));
    }
}
