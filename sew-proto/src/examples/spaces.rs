//! Examples for the wire types of spaces, categories, channel listings and membership.

use super::{
    ID_BOB, ID_CAROL, ID_CATEGORY, ID_CHANNEL, ID_DM, ID_ME, ID_MESSAGE, ID_OTHER_CHANNEL,
    ID_SPACE, channel_full, channel_min, overwrite, space_min, t,
};
use crate::api::channels::*;
use crate::api::members::*;
use crate::api::spaces::*;
use crate::error::*;
use crate::gateway::events::*;
use crate::http::{Page, Patch};
use crate::perms;
use crate::registry::Example;

impl Example for SetInstallRoleRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            SetInstallRoleRequest {
                install_role: SettableInstallRole::Admin,
            },
        )]
    }
}

impl Example for SpaceListQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                SpaceListQuery {
                    scope: Some(SpaceListScope::Joinable),
                    after: Some(ID_SPACE),
                    limit: Some(20),
                },
            ),
            ("min", SpaceListQuery::default()),
        ]
    }
}

impl Example for Page<Space> {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            Page {
                items: vec![Space {
                    my_perms: Some(perms::DEFAULT_EVERYONE),
                    ..space_min()
                }],
                has_more: true,
            },
        )]
    }
}

fn space_member() -> SpaceMember {
    SpaceMember {
        space_id: ID_SPACE,
        member_id: ID_BOB,
        joined_at: t(1_000),
        role_ids: vec![super::ID_ROLE],
        timeout_until: Some(t(600_000)),
    }
}

impl Example for SpaceMember {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", space_member())]
    }
}

impl Example for Page<SpaceMember> {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            Page {
                items: vec![space_member()],
                has_more: false,
            },
        )]
    }
}

impl Example for SpaceMembersQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        let ids = match crate::id::IdList::new(vec![ID_BOB, ID_CAROL]) {
            Ok(l) => l,
            Err(e) => panic!("example IdList: {e}"),
        };
        vec![
            (
                "full",
                SpaceMembersQuery {
                    before: Some(ID_CAROL),
                    after: Some(ID_BOB),
                    around: Some(ID_BOB),
                    limit: Some(20),
                    ids: Some(ids.clone()),
                },
            ),
            ("min", SpaceMembersQuery::default()),
            (
                "ids",
                SpaceMembersQuery {
                    ids: Some(ids),
                    ..SpaceMembersQuery::default()
                },
            ),
        ]
    }
}

impl Example for AddSpaceMembersRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            AddSpaceMembersRequest {
                member_ids: vec![ID_BOB, ID_CAROL],
            },
        )]
    }
}

impl Example for AddSpaceMembersResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            AddSpaceMembersResponse {
                added: vec![SpaceMember {
                    role_ids: vec![],
                    timeout_until: None,
                    ..space_member()
                }],
                already: vec![ID_ME],
                skipped: vec![ID_CAROL],
                banned: vec![ID_CAROL],
            },
        )]
    }
}

impl Example for RemoveSpaceMemberRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", RemoveSpaceMemberRequest { member_id: ID_BOB })]
    }
}

impl Example for TransferSpaceRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", TransferSpaceRequest { member_id: ID_BOB })]
    }
}

impl Example for ReorderCategoriesRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            ReorderCategoriesRequest {
                items: vec![ReorderCategoryItem {
                    category_id: ID_CATEGORY,
                    position: 0,
                }],
            },
        )]
    }
}

impl Example for CategoriesResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            CategoriesResponse {
                categories: vec![Category {
                    id: ID_CATEGORY,
                    space_id: ID_SPACE,
                    name: "Engineering".into(),
                    position: 0,
                    overwrites: vec![overwrite()],
                }],
            },
        )]
    }
}

impl Example for Page<Channel> {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            Page {
                items: vec![channel_full()],
                has_more: false,
            },
        )]
    }
}

impl Example for ChannelListQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                ChannelListQuery {
                    scope: Some(ChannelListScope::Browse),
                    include_archived: true,
                    q: Some("dep".into()),
                    after: Some(ID_CHANNEL),
                    limit: Some(20),
                },
            ),
            ("min", ChannelListQuery::default()),
        ]
    }
}

impl Example for MyChannelsQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MyChannelsQuery {
                    kind: Some(MyChannelsKind::Dm),
                    after: Some(ID_CHANNEL),
                    limit: Some(20),
                },
            ),
            ("min", MyChannelsQuery::default()),
        ]
    }
}

impl Example for MyChannelsPageQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MyChannelsPageQuery {
                    kind: Some(MyChannelsKind::Dm),
                    order: Some(MyChannelsOrder::Activity),
                    after: None,
                    before: Some("48831765147549701.48831765147549711".into()),
                    limit: Some(100),
                },
            ),
            (
                "full_by_id",
                MyChannelsPageQuery {
                    kind: Some(MyChannelsKind::Space),
                    order: Some(MyChannelsOrder::Id),
                    after: Some(ID_CHANNEL),
                    before: None,
                    limit: Some(20),
                },
            ),
            ("min", MyChannelsPageQuery::default()),
        ]
    }
}

/// A named group DM, as `order=activity` lists it.
fn group_dm_named() -> Channel {
    Channel {
        kind: ChannelType::GroupDm,
        name: Some("Launch crew".into()),
        last_message_id: Some(ID_MESSAGE),
        dm_member_ids: vec![ID_ME, ID_BOB, ID_CAROL],
        ..channel_min()
    }
}

impl Example for MyChannelsPage {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MyChannelsPage {
                    items: vec![group_dm_named(), channel_min()],
                    has_more: true,
                    next: Some(format!("{}.{}", ID_DM, ID_DM)),
                },
            ),
            ("min", MyChannelsPage::new(vec![], false)),
        ]
    }
}

impl Example for AddChannelMembersResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            AddChannelMembersResponse {
                added: vec![ChannelMember {
                    channel_id: ID_CHANNEL,
                    member_id: ID_CAROL,
                    joined_at: t(2_000),
                }],
                already: vec![ID_ME],
                skipped: vec![ID_BOB],
                banned: vec![ID_BOB],
            },
        )]
    }
}

impl Example for ReorderChannelsRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            ReorderChannelsRequest {
                items: vec![
                    ReorderChannelItem {
                        channel_id: ID_CHANNEL,
                        category_id: Patch::Set(ID_CATEGORY),
                        position: 0,
                    },
                    ReorderChannelItem {
                        channel_id: ID_OTHER_CHANNEL,
                        category_id: Patch::Keep,
                        position: 1,
                    },
                ],
            },
        )]
    }
}

impl Example for ReorderChannelsResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            ReorderChannelsResponse {
                channels: vec![channel_full()],
            },
        )]
    }
}

impl Example for ConflictDetails {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            ConflictDetails {
                field: "slug".into(),
            },
        )]
    }
}

impl Example for QuotaDetails {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            QuotaDetails {
                limit: "channels_per_space".into(),
                max: 5_000,
            },
        )]
    }
}

impl Example for SpaceMemberAdded {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                SpaceMemberAdded {
                    space_id: ID_SPACE,
                    member_id: ID_BOB,
                    space: Some(Box::new(space_min())),
                    categories: vec![Category {
                        id: ID_CATEGORY,
                        space_id: ID_SPACE,
                        name: "Engineering".into(),
                        position: 0,
                        overwrites: vec![],
                    }],
                },
            ),
            (
                "min",
                SpaceMemberAdded {
                    space_id: ID_SPACE,
                    member_id: ID_BOB,
                    space: None,
                    categories: vec![],
                },
            ),
        ]
    }
}

impl Example for SpaceMemberRemoved {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            SpaceMemberRemoved {
                space_id: ID_SPACE,
                member_id: ID_BOB,
            },
        )]
    }
}

impl Example for CategoryDeleted {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            CategoryDeleted {
                id: ID_CATEGORY,
                space_id: ID_SPACE,
            },
        )]
    }
}
