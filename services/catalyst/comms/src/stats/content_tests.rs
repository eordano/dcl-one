use super::*;
use serde_json::Value;
// Existing structural fixtures do not depend on insertion order.
macro_rules! json {
    ($($tokens:tt)*) => { serde_json::from_value::<crate::content::OrderedJson>(serde_json::json!($($tokens)*)).unwrap() };
}
#[test]
fn hot_scene_fields_preserve_null_coercion_duplicate_counts_and_raw_urls() {
    let scenes = vec![
        json!({"id":"z","metadata":{"scene":{"base":"  -12x,+3,wat,0x10","parcels":["1,2","1,2"]},"display":{"title":null,"navmapThumbnail":"thumb","description":false},"contact":{"name":7},"source":{"projectId":["raw"]}},"content":[{"file":"thumb","hash":"first"},{"file":"thumb","hash":"second"}]}),
        json!({"id":"a","metadata":{"scene":{"base":"4","parcels":[]}}}),
    ];
    let out = content::hot_scenes(&scenes, &[("1,2".into(), 2)], "https://content/").unwrap();
    assert_eq!(
        out,
        r#"[{"id":"z","name":null,"baseCoords":[-12,3,null,0],"usersTotalCount":4,"parcels":[[1,2],[1,2]],"thumbnail":"https://content//contents/first","creator":7,"projectId":["raw"],"description":false},{"id":"a","baseCoords":[4],"usersTotalCount":0,"parcels":[]}]"#
    );
}
#[test]
fn hot_scenes_stably_sort_then_take_100_and_throw_on_invalid_truthy_metadata() {
    let scenes: Vec<_> = (0..105)
        .map(|i| json!({"id":format!("id-{i}"),"metadata":{"scene":{"base":"0,0","parcels":[]}}}))
        .collect();
    let out: Value =
        serde_json::from_str(&content::hot_scenes(&scenes, &[], "url").unwrap()).unwrap();
    assert_eq!(out.as_array().unwrap().len(), 100);
    assert_eq!(out[99]["id"], "id-99");
    for scene in [
        serde_json::json!({"base":1,"parcels":[]}),
        serde_json::json!({"base":"0,0","parcels":true}),
        serde_json::json!({"base":"0,0","parcels":[2]}),
    ] {
        assert!(content::hot_scenes(&[json!({"metadata":{"scene":scene}})], &[], "url").is_err());
    }
    for thumbnail in [
        serde_json::json!(3),
        serde_json::json!(true),
        serde_json::json!([]),
    ] {
        assert!(content::hot_scenes(&[json!({"metadata":{"scene":{"base":"0","parcels":[]},"display":{"navmapThumbnail":thumbnail}}})],&[],"url").is_err());
    }
}
#[test]
fn thumbnail_undefined_null_false_empty_http_and_unmatched_are_distinct() {
    for (thumb, want) in [
        (serde_json::json!(null), Some(serde_json::json!(null))),
        (serde_json::json!(false), Some(serde_json::json!(false))),
        (serde_json::json!(""), Some(serde_json::json!(""))),
        (
            serde_json::json!("http-ish"),
            Some(serde_json::json!("http-ish")),
        ),
        (serde_json::json!("HTTP://x"), None),
    ] {
        let scene = json!({"id":"s","metadata":{"scene":{"base":"0","parcels":[]},"display":{"navmapThumbnail":thumb}}});
        let out: Value =
            serde_json::from_str(&content::hot_scenes(&[scene], &[], "url").unwrap()).unwrap();
        assert_eq!(out[0].get("thumbnail"), want.as_ref());
    }
}
#[tokio::test]
async fn thumbnail_only_sql_matches_full_projection_for_metadata_and_selection_variants() {
    use crate::stats::content::Source;
    let pool = crate::db::scratch::pool(2)
        .await
        .expect("test database required");
    let tag = uuid::Uuid::new_v4();
    let coordinate = 2_000_000_000_000 + (tag.as_u128() % 1_000_000_000) as u64 * 1024;
    let source = content::Content {
        pool: pool.clone(),
        denylist: Arc::new(crate::content::Denylist::default()),
    };
    let mut pointers = Vec::new();
    for (i, thumbnail) in [
        "",
        r#","navmapThumbnail":null"#,
        r#","navmapThumbnail":false"#,
        r#","navmapThumbnail":0"#,
        r#","navmapThumbnail":"""#,
        r#","navmapThumbnail":"thumb.png""#,
        r#","navmapThumbnail":"missing.png""#,
        r#","navmapThumbnail":"http://external/thumb""#,
        r#","navmapThumbnail":"HTTP://external/thumb""#,
        r#","navmapThumbnail":"é.png""#,
        r#","navmapThumbnail":true"#,
        r#","navmapThumbnail":3"#,
        r#","navmapThumbnail":[]"#,
        r#","navmapThumbnail":{}"#,
        r#","navmapThumbnail":"missing.png","navmapThumbnail":"thumb.png""#,
        r#","navmap\u0054humbnail":"thumb.png""#,
    ]
    .into_iter()
    .enumerate()
    {
        let pointer = format!("{},0", coordinate + i as u64);
        let id = format!("thumbnail-{tag}-{i:02}");
        let metadata = format!(
            r#"{{"v":{{"scene":{{"base":"{pointer}","parcels":["{pointer}","{pointer}"]}},"display":{{"title":{{"z":1,"a":2}}{thumbnail}}}}}}}"#
        );
        let deployment: i32 = sqlx::query_scalar("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ('fixture','v3','scene',$1,$2::text::json,to_timestamp(1),$3,now(),'[]'::json) RETURNING id")
            .bind(&id).bind(metadata).bind(vec![pointer.clone()]).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'scene')")
            .bind(&pointer).bind(&id).execute(&pool).await.unwrap();
        for key in [
            "irrelevant.bin",
            "thumb.png",
            "é.png",
            "http://external/thumb",
            "HTTP://external/thumb",
        ] {
            sqlx::query(
                "INSERT INTO content.content_files (deployment,key,content_hash) VALUES ($1,$2,$3)",
            )
            .bind(deployment)
            .bind(key)
            .bind(format!("hash-{key}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        pointers.push(pointer);
    }
    // Request duplicates and reverse order must not change grouped local selection order.
    let mut wanted: Vec<_> = pointers.iter().rev().cloned().collect();
    wanted.push(pointers[0].clone());
    let full: Vec<crate::content::OrderedJson> =
        crate::content::queries::active_entity_docs_with_ordered_metadata_by_pointers(
            &pool, &wanted,
        )
        .await
        .unwrap()
        .iter()
        .map(|doc| serde_json::from_slice(&doc.json).unwrap())
        .collect();
    let thumbnails = source.fetch(&wanted).await.unwrap();
    assert_eq!(thumbnails.len(), full.len());
    let tiles: Vec<_> = pointers
        .iter()
        .map(|pointer| (pointer.clone(), 1))
        .collect();
    for (full, reduced) in full.iter().zip(&thumbnails) {
        assert_eq!(full.get("id"), reduced.get("id"));
        assert_eq!(full.get("metadata"), reduced.get("metadata"));
        assert_eq!(full.get("content").unwrap().as_array().unwrap().len(), 5);
        assert!(reduced.get("content").unwrap().as_array().unwrap().len() <= 1);
        let render = |doc: &crate::content::OrderedJson| {
            content::hot_scenes(std::slice::from_ref(doc), &tiles, "content/")
        };
        match (render(full), render(reduced)) {
            (Ok(full), Ok(reduced)) => assert_eq!(full, reduced),
            (Err(_), Err(_)) => {}
            _ => panic!("thumbnail-only selection changed projection failure semantics"),
        }
    }
    let first = thumbnails[0].get("id").unwrap().as_str().unwrap();
    source.denylist.set_operator([first.to_string()]);
    let filtered = source.fetch(&wanted).await.unwrap();
    assert_eq!(filtered, thumbnails[1..]);
    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id=$1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    source.denylist.set_operator(Vec::new());
    assert_eq!(source.fetch(&wanted).await.unwrap(), thumbnails[1..]);
}

#[tokio::test]
async fn database_projection_matches_active_entities_order_and_sees_immediate_changes() {
    use crate::stats::content::Source;
    let pool = crate::db::scratch::pool(2)
        .await
        .expect("CATALYST_TEST_DATABASE_URL required for statistics content fixture");
    let tag = uuid::Uuid::new_v4().to_string();
    let pointers = [format!("1{tag},0"), format!("2{tag},0")];
    let ids = [format!("z-{tag}"), format!("a-{tag}")];
    for i in 0..2 {
        let metadata = serde_json::json!({"v":{"scene":{"base":"0,0","parcels":[pointers[i]]},"display":{"title":"before","navmapThumbnail":"thumb.png"}}});
        let deployment:i64=sqlx::query_scalar("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ('0x0000000000000000000000000000000000000001','v3','scene',$1,$2,timezone('UTC',now()),$3,timezone('UTC',now()),'[]'::json) RETURNING id::bigint")
            .bind(&ids[i]).bind(metadata).bind(vec![pointers[i].clone()]).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'scene')").bind(&pointers[i]).bind(&ids[i]).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.content_files (deployment,key,content_hash) VALUES ($1,'thumb.png','hash')").bind(deployment as i32).execute(&pool).await.unwrap();
    }
    let source = content::Content {
        pool: pool.clone(),
        denylist: Arc::new(crate::content::Denylist::default()),
    };
    let wanted = vec![
        pointers[1].clone(),
        pointers[0].clone(),
        pointers[0].clone(),
    ];
    let docs = source.fetch(&wanted).await.unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(
        docs.iter()
            .map(|d| d.get("id").unwrap().as_str().unwrap())
            .collect::<Vec<_>>(),
        ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let expected = crate::content::queries::active_entity_docs_by_pointers(&pool, &wanted)
        .await
        .unwrap()
        .iter()
        .map(|d| d.to_value().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(&docs).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    let hot = content::hot_scenes(
        &docs,
        &[(pointers[0].clone(), 1), (pointers[1].clone(), 1)],
        "content/",
    )
    .unwrap();
    assert!(hot.contains("content//contents/hash"));
    sqlx::query("UPDATE content.deployments SET entity_metadata=jsonb_set(entity_metadata::jsonb,'{v,display,title}','\"after\"'::jsonb)::json WHERE entity_id=$1").bind(&ids[0]).execute(&pool).await.unwrap();
    assert_eq!(
        source.fetch(&wanted).await.unwrap()[0]
            .get("metadata")
            .unwrap()
            .get("display")
            .unwrap()
            .get("title")
            .unwrap()
            .as_str(),
        Some("after")
    );
    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id=$1")
        .bind(&ids[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(source.fetch(&wanted).await.unwrap().len(), 1);
    assert!(source.fetch(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn selected_sql_content_applies_shared_denylist_before_top_100() {
    use crate::stats::content::Source;
    let pool = crate::db::scratch::pool(2)
        .await
        .expect("test database required");
    let denylist = Arc::new(crate::content::Denylist::default());
    let source = content::Content {
        pool: pool.clone(),
        denylist: denylist.clone(),
    };
    let tag = uuid::Uuid::new_v4();
    let coordinate = 1_000_000_000_000 + (tag.as_u128() % 1_000_000_000) as u64 * 1024;
    let mut pointers = Vec::new();
    let mut ids = Vec::new();
    // Synthetic binder rows exercise selected-source SQL, not deployment admission.
    for i in 0..102 {
        let id = format!("scene-{tag}-{i:03}");
        let pointer = format!("{},0", coordinate + i);
        let metadata = format!(
            r#"{{"v":{{"scene":{{"base":"{pointer}","parcels":["{pointer}"]}},"display":{{"title":{{"z":1,"a":2}}}}}}}}"#
        );
        sqlx::query("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ('0x0000000000000000000000000000000000000001','v3','scene',$1,$2::json,timezone('UTC',now()),$3,timezone('UTC',now()),'[]'::json)")
            .bind(&id).bind(metadata).bind(vec![pointer.clone()]).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'scene')")
            .bind(&pointer).bind(&id).execute(&pool).await.unwrap();
        pointers.push(pointer);
        ids.push(id);
    }
    denylist.set_operator([ids[0].clone(), ids[1].clone()]);
    let documents = source.fetch(&pointers).await.unwrap();
    assert_eq!(
        documents.len(),
        100,
        "denied selected scenes must be removed"
    );
    let selected_ids: Vec<_> = documents
        .iter()
        .map(|doc| doc.get("id").unwrap().as_str().unwrap())
        .collect();
    let expected: Vec<_> = ids[2..].iter().map(String::as_str).collect();
    assert_eq!(
        selected_ids, expected,
        "filter preserves SQL survivor order"
    );
    let tiles: Vec<_> = pointers
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), if i < 2 { 1000 } else { 1 }))
        .collect();
    let rendered = content::hot_scenes(&documents, &tiles, "").unwrap();
    let hot: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hot.as_array().unwrap().len(),
        100,
        "filter precedes top-100 selection"
    );
    assert!(
        hot.as_array()
            .unwrap()
            .iter()
            .all(|scene| scene["usersTotalCount"] == 1)
    );
    assert!(rendered.contains(r#""name":{"z":1,"a":2}"#));
    denylist.set_operator(ids.clone());
    assert!(
        source.fetch(&pointers).await.unwrap().is_empty(),
        "shared policy update affects next pass"
    );
    denylist.set_operator(Vec::new());
    assert_eq!(source.fetch(&pointers).await.unwrap().len(), 102);
}
#[test]
fn parse_int_uses_ecmascript_whitespace_and_decimal_prefixes() {
    let docs = vec![
        json!({"metadata":{"scene":{"base":"\u{0085}1,\u{feff}2,-0,1e3,+0x12,9007199254740993","parcels":[]}}}),
    ];
    assert_eq!(
        content::hot_scenes(&docs, &[], "url").unwrap(),
        r#"[{"baseCoords":[null,2,0,1,0,9007199254740992],"usersTotalCount":0,"parcels":[]}]"#
    );
}
struct CountingSource {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}
#[async_trait::async_trait]
impl content::Source for CountingSource {
    async fn fetch(
        &self,
        pointers: &[String],
    ) -> Result<Vec<crate::content::OrderedJson>, content::Error> {
        self.calls.lock().unwrap().push(pointers.to_vec());
        Ok(vec![json!({"id":"scene"})])
    }
}
#[tokio::test]
async fn tile_documents_skips_fetch_for_empty_tiles_and_fetches_once_in_tile_order() {
    let source = CountingSource {
        calls: Default::default(),
    };
    let docs = content::tile_documents(&source, &[]).await.unwrap();
    assert!(docs.is_empty());
    assert!(source.calls.lock().unwrap().is_empty());

    let tiles = [("3,4".to_string(), 2), ("1,2".to_string(), 5)];
    let docs = content::tile_documents(&source, &tiles).await.unwrap();
    assert_eq!(docs, vec![json!({"id":"scene"})]);
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![vec!["3,4".to_string(), "1,2".to_string()]]
    );
}
