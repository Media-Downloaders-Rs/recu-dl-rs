use personal_downloaders::sites::recu;
use recu::Kind;
const PROFILE: &str = "https://es.recu.me/performer/fixture_performer_000";
#[test]
fn pages_and_categories_are_scoped() {
    assert_eq!(recu::normalize(PROFILE).unwrap(), PROFILE);
    assert!(
        recu::normalize("https://es.recu.me.evil.test/performer/fixture_performer_000").is_err()
    );
    assert!(recu::normalize("https://es.recu.me/performer/fixture_performer_000?x=1").is_err());
    let recording = "https://es.recu.me/fixture_performer_000/video/900000001/play";
    let html = format!(
        "<link rel='canonical' href='{PROFILE}/latest'><div class='performer-page-videos'><div class='video-thumb' data-id='900000001' data-performer='fixture_performer_000'><a href='/fixture_performer_000/video/900000001/play'>video</a></div><div class='video-thumb' data-id='99' data-performer='other'><a href='/other/video/99/play'>other</a></div></div><ul class='pagination' data-pg-count='11'></ul><div class='video-thumb' data-id='55' data-performer='fixture_performer_000'><a href='/fixture_performer_000/video/55/play'>suggested</a></div>"
    );
    let (items, pages) = recu::parse_catalog(&html, PROFILE, Kind::Recordings, 1).unwrap();
    assert_eq!(pages, 11);
    assert_eq!(items, vec![("900000001".into(), recording.into())]);
    let clip_html = String::from(
        "<link rel='canonical' href='https://es.recu.me/clips/fixture_performer_000'><div class='performer-page-clips'><div class='video-thumb' data-id='900011' data-performer='fixture_performer_000'><a href='/clips/900011/play?clips_type=performer_clips'>clip</a></div></div>",
    );
    let (clips, pages) = recu::parse_catalog(&clip_html, PROFILE, Kind::Clips, 1).unwrap();
    assert_eq!(pages, 1);
    assert_eq!(
        clips,
        vec![(
            "900011".into(),
            "https://es.recu.me/clips/900011/play".into()
        )]
    );
}
#[test]
fn player_access_is_explicit() {
    let denied = serde_json::json!({"status":false,"reason":"shall_signin","player":null});
    assert!(
        recu::parse_player_json(&denied)
            .unwrap_err()
            .to_string()
            .contains("shall_signin")
    );
    let allowed = serde_json::json!({"status":true,"player":"<video><source src=\"https://f42.mediafront.net/vod/fixture_performer_000/2000-01-01,00-00/max.m3u8?uid=x\"></video>"});
    assert!(
        recu::parse_player_json(&allowed)
            .unwrap()
            .contains("max.m3u8")
    );
}

#[test]
fn hls_master_and_segments_keep_owner_and_signature() {
    let source = "https://f41.mediafront.net/vod/fixture_performer_000/2000-01-01,00-00/index.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2";
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=600000,NAME=240p\n240p.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2\n#EXT-X-STREAM-INF:BANDWIDTH=5070000,NAME=max\nmax.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2\n";
    let variant = recu::parse_master(master, source, PROFILE).unwrap();
    assert!(variant.contains("max.m3u8"));
    let body = "#EXTM3U\n#EXTINF:2.0,\nseg-1-v1-a1.ts?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2\n#EXT-X-ENDLIST\n";
    let segments = recu::parse_segments(body, &variant, PROFILE).unwrap();
    assert_eq!(segments.len(), 1);
    assert!(segments[0].ends_with("&check=a58euaxp7860"));
    assert!(recu::validate_hls(&segments[0], "https://es.recu.me/performer/other").is_err());
    assert!(
        recu::parse_segments(
            &body.replace("seg-1-v1-a1.ts", "https://evil.test/seg-1-v1-a1.ts"),
            &variant,
            PROFILE
        )
        .is_err()
    );
}

#[test]
fn source_identity_survives_signed_url_rotation() {
    use personal_downloaders::{File, state::source_key};
    let page = File {
        id: "grabaciones:900000001".into(),
        post: PROFILE.into(),
        url: "https://es.recu.me/fixture_performer_000/video/900000001/play".into(),
        path: "Recu.me - fixture_performer_000/grabaciones/900000001.ts".into(),
        ..Default::default()
    };
    let mut signed = page.clone();
    signed.url = "https://f41.mediafront.net/vod/fixture_performer_000/2000-01-01,00-00/index.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2".into();
    assert_eq!(source_key(&page).unwrap(), source_key(&signed).unwrap());
    signed.url = "https://evil.test/video.mp4".into();
    assert!(source_key(&signed).is_err());
}

#[test]
fn clip_player_denials_and_owner_checks() {
    assert!(recu::parse_clip_json(&serde_json::json!("shall_signin")).is_err());
    let clip = serde_json::json!({"clip":{"videoUrl":"https://f98.mediafront.net/vod/fixture_performer_000/clip-900011/clip-900011.mp4?uid=x"}});
    let url = recu::parse_clip_json(&clip).unwrap();
    recu::validate_clip_source(&url, PROFILE, "900011").unwrap();
    assert!(recu::validate_clip_source(&url, PROFILE, "545000").is_err());
    assert!(
        recu::validate_clip_source(
            "https://f98.mediafront.net.evil.test/vod/fixture_performer_000/clip-900011.mp4",
            PROFILE,
            "900011"
        )
        .is_err()
    );
}

#[test]
fn clip_hls_uses_its_own_signed_segments() {
    let source = "https://f41.mediafront.net/vod/fixture_performer_000/clip-900011/index.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2";
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\n720p.m3u8?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2\n";
    let variant = recu::parse_clip_master(master, source, PROFILE, "900011").unwrap();
    let body = "#EXTM3U\n#EXTINF:2.0,\nseg-1-v1-a1.ts?uid=ZZuaxpFIXTURE&mid=M&origin=108&request_id=a58efixture-DFW&p=6002&expires=1794427860&md5=S&secret=v2\n#EXT-X-ENDLIST\n";
    let segments = recu::parse_clip_segments(body, &variant, PROFILE, "900011").unwrap();
    assert_eq!(segments.len(), 1);
    assert!(segments[0].ends_with("&check=a58euaxp7860"));
    assert!(recu::parse_clip_segments(body, &variant, PROFILE, "545000").is_err());
}

#[test]
fn signed_clip_source_remains_valid_for_state_and_policy() {
    use personal_downloaders::{
        File, Site,
        state::source_key,
        transport::{SitePolicy, UrlPolicy},
    };
    let page = File {
        id: "clips:900011".into(),
        post: PROFILE.into(),
        url: "https://es.recu.me/clips/900011/play".into(),
        path: "Recu.me - fixture_performer_000/clips/900011.mp4".into(),
        ..Default::default()
    };
    let mut signed = page.clone();
    signed.url =
        "https://f98.mediafront.net/vod/fixture_performer_000/clip-900011/clip-900011.mp4?uid=fixture"
            .into();
    assert_eq!(source_key(&page).unwrap(), source_key(&signed).unwrap());
    SitePolicy(Site::Recu)
        .check(&url::Url::parse(&signed.url).unwrap(), PROFILE)
        .unwrap();
}

#[test]
fn catalog_cards_keep_recent_date_and_safe_thumbnail() {
    let thumb = "https://f98.mediafront.net/l30/fixture_performer_000/2000-01-01,00-00.jpg?expires=1794417300&md5=fixture";
    let html = format!(
        "<h1><a class='performer-link' href='/performer/fixture_performer_000'>fixture_performer_000</a></h1><div class='performer-page-videos'><div class='video-thumb' data-id='900000001' data-performer='fixture_performer_000'><a href='/fixture_performer_000/video/900000001/play'><div class='video-splash' data-src='{thumb}'></div></a><span class='video-date'>2026-11-10 19:52</span></div></div>"
    );
    let (cards, _) = recu::parse_catalog_cards(&html, PROFILE, Kind::Recordings, 1).unwrap();
    assert_eq!(cards[0].thumbnail.as_deref(), Some(thumb));
    assert_eq!(cards[0].published_at.as_deref(), Some("2026-11-10 19:52"));
    let bad = html.replace(thumb, "https://evil.test/preview.jpg");
    let (cards, _) = recu::parse_catalog_cards(&bad, PROFILE, Kind::Recordings, 1).unwrap();
    assert!(cards[0].thumbnail.is_none());
}

#[test]
fn recent_recordings_precede_older_recordings_and_clips() {
    use personal_downloaders::File;
    let make = |id: &str, date: Option<&str>| File {
        id: id.into(),
        published_at: date.map(str::to_owned),
        ..Default::default()
    };
    let mut files = vec![
        make("clips:900", None),
        make("grabaciones:100", Some("2026-11-09 18:00")),
        make("grabaciones:101", Some("2026-11-10 10:00")),
        make("clips:901", None),
    ];
    recu::sort_recent(&mut files);
    assert_eq!(
        files
            .iter()
            .map(|file| file.id.as_str())
            .collect::<Vec<_>>(),
        [
            "grabaciones:101",
            "grabaciones:100",
            "clips:901",
            "clips:900"
        ]
    );
}
