//! Compile-guard: proves the canister-client symbols the merged upload routes
//! depend on resolve under the current pinned commit. Pure type/const references;
//! no network. The imports themselves ARE the guard — if a symbol is renamed or
//! removed in yral-common `main`, this test fails to compile and CI catches it.
#![allow(unused_imports, dead_code)]

use yral_canisters_client::ic::{USER_INFO_SERVICE_ID, USER_POST_SERVICE_ID};
use yral_canisters_client::user_info_service::{Result6, UserInfoService};
use yral_canisters_client::user_post_service::{
    PostDetailsFromFrontendV1, PostStatus, PostStatusFromFrontend, Result2, Result_,
    UserPostService,
};

#[test]
fn canister_symbols_resolve() {
    let _ = USER_INFO_SERVICE_ID;
    let _ = USER_POST_SERVICE_ID;
    let _draft = PostStatusFromFrontend::Draft;
    let _pub = PostStatusFromFrontend::Published;
    let _uploaded = PostStatus::Uploaded;
}
