mod helpers;

use std::sync::Arc;

use helpers::FakeHelix;
use twitch_1337_web::helix::HelixClient;

#[tokio::test]
async fn is_follower_reports_membership() {
    let helix: Arc<dyn HelixClient> = Arc::new(FakeHelix {
        moderators: vec![],
        followers: vec!["42".into()],
        users: Default::default(),
    });
    assert!(helix.is_follower("b123", "42").await.unwrap());
    assert!(!helix.is_follower("b123", "99").await.unwrap());
}
