//! Worlds mirror upstream.
use super::*;

/// `{source}/world/{name}/{tail}` with the name as one percent-encoded path segment, so no
/// name can add a segment, a query or a fragment to the upstream request.
pub(super) fn world_url(source: &str, name: &str, tail: &str) -> Result<String> {
    if matches!(name, "" | "." | "..") {
        bail!("{name:?} is not a world name");
    }
    let mut url = url::Url::parse(source).with_context(|| format!("{source} is not a URL"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("{source} cannot carry a path"))?
        .pop_if_empty()
        .extend(["world", name, tail]);
    Ok(url.into())
}

/// `{source}/world/{name}/permissions` (at most [`MAX_PERMISSIONS_BYTES`]); anything but a 2xx
/// JSON answer is an error.
async fn fetch_permissions(client: &crate::http::Outbound, url: &str) -> Result<Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} answered {}", resp.status());
    }
    serde_json::from_slice(&read_body_capped(resp, MAX_PERMISSIONS_BYTES).await?)
        .with_context(|| format!("parsing {url}"))
}

/// The world's access setting at the source ([`parse_upstream_access`]), read on every pass that
/// reaches the world, a changed one or not.
pub(super) async fn upstream_access(
    client: &crate::http::Outbound,
    source: &str,
    name: &str,
) -> Result<AccessSetting> {
    let url = world_url(source, name, "permissions")?;
    parse_upstream_access(&fetch_permissions(client, &url).await?)
}

/// The access setting the source's `/permissions` reports (`permissions.access`, upstream's
/// `getAccessForWorld` without secrets), as this node stores it. The shared secret's hash is not
/// on the wire, so a shared-secret world is stored with [`NO_SECRET_MATCHES`]: no secret admits
/// and only the owner enters. An allow-list keeps the wallets and community ids that parse. Any
/// other answer is an error, so a world is never stored open on a doubtful reading.
pub(super) fn parse_upstream_access(body: &Value) -> Result<AccessSetting> {
    let access = &body["permissions"]["access"];
    let strings = |key: &str| -> Vec<String> {
        access[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(match access["type"].as_str() {
        Some("unrestricted") => AccessSetting::Unrestricted,
        Some("shared-secret") => AccessSetting::SharedSecret {
            secret_hash: NO_SECRET_MATCHES.to_string(),
        },
        Some("nft-ownership") => AccessSetting::NftOwnership {
            nft: access["nft"]
                .as_str()
                .filter(|nft| !nft.is_empty())
                .context("the nft-ownership access names no nft")?
                .to_string(),
        },
        Some("allow-list") => AccessSetting::AllowList {
            wallets: strings("wallets"),
            communities: strings("communities"),
        },
        other => bail!("the permissions name no known access type: {other:?}"),
    })
}

/// Who published each scene of the world a read-through fills, by the chain its listing row
/// carries.
pub(super) struct Signatures<'a> {
    verify: &'a Verify<'a>,
    world: WorldName,
    /// `{source}/world/{name}/permissions` when upstream grants are trusted.
    permissions_url: Option<url::Url>,
    /// That answer, asked at most once per fill.
    grant: tokio::sync::OnceCell<Result<UpstreamGrant, String>>,
}

/// What the upstream worlds server says of a world's deployment rights: its owner and the
/// addresses on the deployment allow-list.
#[derive(Debug, PartialEq)]
pub(super) struct UpstreamGrant {
    pub(super) owner: Address,
    pub(super) deployers: Vec<Address>,
}

/// `{"owner": "0x..", "permissions": {"deployment": {"type": "allow-list", "wallets": [..]}}}`;
/// any other deployment type grants no one.
pub(super) fn parse_upstream_grant(body: &Value) -> Result<UpstreamGrant> {
    let owner = body["owner"]
        .as_str()
        .context("the permissions name no owner")?;
    let owner = Address::parse(owner).map_err(|e| anyhow!("the permissions owner: {e}"))?;
    let deployment = &body["permissions"]["deployment"];
    let deployers = if deployment["type"].as_str() == Some("allow-list") {
        deployment["wallets"]
            .as_array()
            .context("the deployment allow-list has no wallets")?
            .iter()
            .filter_map(|w| w.as_str().and_then(|w| Address::parse(w).ok()))
            .collect()
    } else {
        Vec::new()
    };
    Ok(UpstreamGrant { owner, deployers })
}

/// A scene's chain and the signer the policy allowed.
pub(super) struct Signed {
    pub(super) chain: AuthChain,
    pub(super) json: Value,
    pub(super) signer: Address,
    /// The scene's size as the upstream lists it: a read-through fetches its files on demand,
    /// so this is the figure `/scenes` reports until a pass fetches them.
    pub(super) size: i64,
}

impl<'a> Signatures<'a> {
    pub(super) fn new(
        verify: &'a Verify<'a>,
        settings: &MirrorSettings,
        name: &str,
    ) -> Result<Self> {
        let source = settings.source.as_str();
        let permissions_url = match settings.trust_upstream_grants {
            true => Some(url::Url::parse(&world_url(source, name, "permissions")?)?),
            false => None,
        };
        Ok(Self {
            verify,
            world: WorldName::parse(name)?,
            permissions_url,
            grant: tokio::sync::OnceCell::new(),
        })
    }

    /// A signer the NAME holder did not sign as may still have been granted the world upstream.
    /// That grant is the upstream worlds server's word, not a signature this node checks (the
    /// owner's signed grant request is not published): it is trusted only under
    /// `WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS`, only for a read-through fill (a local
    /// deployment keeps the policy's own grants), and only when the owner the upstream names is
    /// the one this node's own NAME authority names. `Ok(Err(why))` is a refusal on the answers
    /// this node has (grants untrusted, a signer off the allow-list, an owner the NAME authority
    /// definitely does not name). `Err` is a fault that fails the whole fill: a `/permissions`
    /// answer that is unreachable, not 2xx or unparsable, or a NAME authority that cannot
    /// answer.
    pub(super) async fn upstream_grant(
        &self,
        client: &crate::http::Outbound,
        signer: &Address,
    ) -> Result<std::result::Result<(), String>> {
        let Some(url) = &self.permissions_url else {
            return Ok(Err(
                "upstream grants are not trusted (WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS)"
                    .into(),
            ));
        };
        let grant = self
            .grant
            .get_or_init(|| async {
                fetch_permissions(client, url.as_str())
                    .await
                    .and_then(|body| parse_upstream_grant(&body))
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .as_ref()
            .map_err(|e| anyhow!("{e}"))?;
        if !grant.deployers.contains(signer) {
            return Ok(Err(format!(
                "{signer} is not on the upstream deployment allow-list"
            )));
        }
        match self
            .verify
            .policy
            .may_have_published(&self.world, &grant.owner)
            .await
        {
            PermissionDecision::Allowed => Ok(Ok(())),
            PermissionDecision::Refused(why) => {
                let refusal = format!(
                    "the upstream names {} as the owner, which this node's NAME authority does not: {why}",
                    grant.owner
                );
                if definite(&why) {
                    Ok(Err(refusal))
                } else {
                    bail!(refusal)
                }
            }
        }
    }

    /// The chain the listing carries for the scene and its signer, refused unless the policy says
    /// the signer may have published this world. Asked before the entity is fetched.
    pub(super) async fn signer(
        &self,
        client: &crate::http::Outbound,
        scene: &ListedScene,
    ) -> Result<Signed> {
        let cid = scene.id.as_str();
        let (json, chain_bytes) = (scene.chain.clone(), scene.chain_bytes);
        let size = scene.size.unwrap_or(0);
        if chain_bytes > MAX_CHAIN_BYTES {
            return Err(Unacceptable(format!(
                "the auth chain of scene {cid} is {chain_bytes} bytes, over {MAX_CHAIN_BYTES}"
            ))
            .into());
        }
        let chain: AuthChain = serde_json::from_value(json.clone()).map_err(|e| {
            Unacceptable(format!("the auth chain of scene {cid} is malformed: {e}"))
        })?;
        let signer = chain
            .first()
            .ok_or_else(|| Unacceptable(format!("the auth chain of scene {cid} is empty")))
            .and_then(|link| {
                Address::parse(&link.payload).map_err(|e| {
                    Unacceptable(format!(
                        "the auth chain of scene {cid} names no signer: {e}"
                    ))
                })
            })?;
        match self
            .verify
            .policy
            .may_have_published(&self.world, &signer)
            .await
        {
            PermissionDecision::Allowed => Ok(Signed {
                chain,
                json,
                signer,
                size,
            }),
            PermissionDecision::Refused(why) => match self.upstream_grant(client, &signer).await? {
                Ok(()) => Ok(Signed {
                    chain,
                    json,
                    signer,
                    size,
                }),
                Err(grant) => {
                    let refusal = format!(
                        "{signer} may not have published scene {cid} to {}: {why}; not granted upstream: {grant}",
                        self.world
                    );
                    if definite(&why) {
                        Err(Unacceptable(refusal).into())
                    } else {
                        bail!(refusal)
                    }
                }
            },
        }
    }

    /// The fetched entity is a scene of this world and the chain signs its id with ECDSA links
    /// only, ephemeral keys judged at the entity's timestamp. The upstream worlds server accepts
    /// no contract-wallet (EIP-1654) link, so one refuses here; the peer validator content sync
    /// runs would take it on the peer's word.
    pub(super) async fn check(&self, cid: &str, entity: &Value, signed: &Signed) -> Result<()> {
        if entity["type"].as_str() != Some("scene") {
            bail!("entity {cid} is not a scene");
        }
        let declared = entity["metadata"]["worldConfiguration"]["name"]
            .as_str()
            .unwrap_or_default();
        if !declared.eq_ignore_ascii_case(self.world.key()) {
            bail!(
                "scene {cid} names the world {declared:?}, not {}",
                self.world
            );
        }
        let timestamp = crate::core::store::entity_timestamp_ms(entity)
            .with_context(|| format!("entity {cid} has no timestamp"))?;
        verify_auth_chain(&signed.chain, cid, Some(timestamp))
            .with_context(|| format!("the auth chain of scene {cid} does not verify"))?;
        Ok(())
    }
}

/// A scene whose chain this node will not accept, on answers it has: an over-size, malformed or
/// empty chain, or a signer the NAME authority says may not publish the world and no trusted
/// upstream grant covers. An answer that could not be read is a plain error instead.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct Unacceptable(String);

/// A refusal the authority gave on what it read, not one because it could not read.
fn definite(why: &AuthorityNotEstablished) -> bool {
    matches!(why, AuthorityNotEstablished::RefusedLacksAuthority { .. })
}

/// The settings a source stores for a world (`GET {source}/world/{name}/settings`, public and
/// shaped for mirrors upstream: `toSnakeCaseSettings` of `getWorldSettings`), as this node can
/// store them. The default is no answer: every column stays as it is.
#[derive(Debug, Default, PartialEq)]
pub(super) struct WorldSettingsCopy {
    /// The source answered; the text columns and categories are then written, an absent key
    /// clearing its column (upstream leaves out an empty text and NULL categories).
    pub(super) present: bool,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    /// One of the ratings the column accepts, else none.
    pub(super) content_rating: Option<String>,
    /// The first [`MAX_CATEGORIES`] strings.
    pub(super) categories: Option<Vec<String>>,
    /// The source's key, a canonical CID or the SHA-256 hex of an upload; [`mirrored_thumbnail`]
    /// turns it into the CID of the bytes held here before it is stored.
    pub(super) thumbnail_hash: Option<String>,
    /// The source names a thumbnail this pass could not fetch: the stored one stays.
    pub(super) thumbnail_unread: bool,
    /// `Some(None)` clears: upstream answers `null` for no fixed hour.
    pub(super) skybox_time: Option<Option<i32>>,
    pub(super) single_player: Option<bool>,
    pub(super) show_in_places: Option<bool>,
    pub(super) settings_version: Option<i64>,
}

impl WorldSettingsCopy {
    pub(super) fn parse(body: &Value) -> Option<Self> {
        let body = body.as_object()?;
        let text = |key: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(String::from)
        };
        Some(Self {
            present: true,
            title: text("title"),
            description: text("description"),
            content_rating: text("content_rating").filter(|r| VALID_RATINGS.contains(&r.as_str())),
            categories: body.get("categories").and_then(Value::as_array).map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .take(MAX_CATEGORIES)
                    .map(String::from)
                    .collect()
            }),
            thumbnail_hash: text("thumbnail_hash")
                .filter(|h| is_canonical_cid(h) || raw_cid_v1_from_sha256_hex(h).is_some()),
            thumbnail_unread: false,
            skybox_time: match body.get("skybox_time") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(t) => t.as_f64().and_then(storable_skybox_time).map(Some),
            },
            single_player: body.get("single_player").and_then(Value::as_bool),
            show_in_places: body.get("show_in_places").and_then(Value::as_bool),
            settings_version: body
                .get("settings_version")
                .and_then(Value::as_i64)
                .filter(|v| *v >= 0),
        })
    }
}

/// `{source}/world/{name}/settings`, read on every pass that reaches the world (upstream edits
/// settings and access without a deployment, bumping `settings_version`), with its thumbnail
/// resolved by [`mirrored_thumbnail`]. `None` (no answer, a refusal or a body that is not a
/// settings object) leaves the stored settings as they are.
pub(super) async fn upstream_settings(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Storage,
    source: &str,
    name: &str,
) -> Option<WorldSettingsCopy> {
    let url = world_url(source, name, "settings").ok()?;
    let read = async {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            bail!("answered {}", resp.status());
        }
        let body: Value =
            serde_json::from_slice(&read_body_capped(resp, MAX_SETTINGS_BYTES).await?)?;
        WorldSettingsCopy::parse(&body).context("not a settings object")
    };
    let mut copy = match read.await {
        Ok(copy) => copy,
        Err(e) => {
            tracing::warn!(world = %name, error = %e, "keeping the stored settings: GET {url}");
            return None;
        }
    };
    if let Some(key) = copy.thumbnail_hash.take() {
        match mirrored_thumbnail(client, pool, storage, source, name, &key).await {
            Ok(cid) => copy.thumbnail_hash = cid,
            Err(e) => {
                tracing::warn!(world = %name, thumbnail = %key, error = %e, "keeping the stored thumbnail");
                copy.thumbnail_unread = true;
            }
        }
    }
    Some(copy)
}

/// The CID under which this node holds the thumbnail the source stores under `key`, fetched
/// from `{source}/contents/{key}` when it is not held, or `None` when the source's file is no
/// image this node serves (the world then has no thumbnail here). A failed read or store is an
/// error, so the stored thumbnail stays. A SHA-256 hex key (an upload upstream,
/// served only by the source) is checked against the bytes' SHA-256 and stored under their
/// CIDv1, the key every route here serves; a row whose stored thumbnail already holds those
/// bytes keeps it without a fetch (a thumbnail over one 256 KiB chunk has a CIDv1 that is not
/// the raw-leaf CID of the digest). The bytes must be an image, as for a local write
/// ([`storable_thumbnail_hash`]).
pub(super) async fn mirrored_thumbnail(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Storage,
    source: &str,
    name: &str,
    key: &str,
) -> Result<Option<String>> {
    let sha256_is_key = |bytes: &[u8]| {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes)) == key
    };
    let resolve = async {
        let held = |cid: String| async move {
            Ok::<_, anyhow::Error>(storage.exist(&cid).await?.then_some(cid))
        };
        let cid = if is_canonical_cid(key) {
            held(key.to_string()).await?
        } else {
            let raw = raw_cid_v1_from_sha256_hex(key).context("not a thumbnail key")?;
            match held(raw).await? {
                Some(cid) => Some(cid),
                None => {
                    let stored = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT thumbnail_hash FROM worlds.worlds \
                         WHERE name = lower($1) AND mirror_source IS NOT NULL",
                    )
                    .bind(name)
                    .fetch_optional(pool)
                    .await?
                    .flatten()
                    .filter(|cid| is_canonical_cid(cid));
                    match stored {
                        Some(cid) => match storage.retrieve(&cid).await? {
                            Some(bytes) if sha256_is_key(&bytes) => Some(cid),
                            _ => None,
                        },
                        None => None,
                    }
                }
            }
        };
        let cid = match cid {
            Some(cid) => cid,
            None => {
                let url = contents_url(source, key)?;
                let resp = client.get(&url).send().await?;
                if !resp.status().is_success() {
                    bail!("GET {url} answered {}", resp.status());
                }
                let bytes = read_body_capped(resp, MAX_THUMBNAIL_BYTES).await?;
                let cid = if is_canonical_cid(key) {
                    if !verify_hash(&bytes, key) {
                        bail!("{url}: body does not hash to {key}");
                    }
                    key.to_string()
                } else {
                    if !sha256_is_key(&bytes) {
                        bail!("{url}: body is not the SHA-256 {key}");
                    }
                    hash_bytes_v1(&bytes)
                };
                if detect_image_format(&bytes).is_none() {
                    tracing::debug!(world = %name, thumbnail = %key, "{url} is not an image");
                    return Ok(None);
                }
                storage.store(&cid, Bytes::from(bytes)).await?;
                cid
            }
        };
        Ok::<_, anyhow::Error>(storable_thumbnail_hash(storage, &cid).await)
    };
    resolve.await
}

/// `{source}/contents/{key}`.
fn contents_url(source: &str, key: &str) -> Result<String> {
    let mut url = url::Url::parse(source).with_context(|| format!("{source} is not a URL"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("{source} cannot carry a path"))?
        .pop_if_empty()
        .extend(["contents", key]);
    Ok(url.into())
}

/// One row of the source's `/world/{name}/scenes` listing: the scene id and what its deployment
/// stored there (`toWorldScene`). A pass keeps the times, and the chain and deployer once they
/// check ([`listed_signature`]); a read-through keeps the times and its own checked chain.
pub(super) struct ListedScene {
    pub(super) id: String,
    /// `createdAt` and `updatedAt` in UTC; `None` when absent or not RFC 3339.
    pub(super) created_at: Option<chrono::NaiveDateTime>,
    pub(super) updated_at: Option<chrono::NaiveDateTime>,
    pub(super) deployer: Option<String>,
    /// `deploymentAuthChain`, `Null` when absent or over [`MAX_CHAIN_BYTES`].
    pub(super) chain: Value,
    /// The size of that chain's JSON, so a fill can tell an over-size chain from a missing one.
    pub(super) chain_bytes: usize,
    /// `size` (a decimal string upstream), `None` unless a non-negative integer.
    pub(super) size: Option<i64>,
}

impl ListedScene {
    fn from_row(id: &str, row: &Value) -> Self {
        let time = |key: &str| {
            row[key]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.naive_utc())
        };
        let chain = &row["deploymentAuthChain"];
        let chain_bytes = serde_json::to_vec(chain).map_or(usize::MAX, |bytes| bytes.len());
        let chain = match chain_bytes <= MAX_CHAIN_BYTES {
            true => chain.clone(),
            false => Value::Null,
        };
        let size = match &row["size"] {
            Value::String(s) => s.parse().ok(),
            other => other.as_i64(),
        };
        Self {
            id: id.to_string(),
            created_at: time("createdAt"),
            updated_at: time("updatedAt"),
            deployer: row["deployer"].as_str().map(String::from),
            chain,
            chain_bytes,
            size: size.filter(|s| *s >= 0),
        }
    }
}

/// The listed chain and deployer of a scene a pass stores, kept only when the chain is ECDSA
/// links that sign `entity`'s id (ephemeral keys judged at its timestamp) and its signer is the
/// listed deployer; `None` keeps the pass's empty chain and zero address. Nothing grants rights
/// from these columns, so a checked copy changes no authority here.
pub(super) fn listed_signature(listed: &ListedScene, entity: &Value) -> Option<(Value, String)> {
    let deployer = Address::parse(listed.deployer.as_deref()?).ok()?;
    let chain: AuthChain = serde_json::from_value(listed.chain.clone()).ok()?;
    if Address::parse(&chain.first()?.payload).ok()? != deployer {
        return None;
    }
    let timestamp = crate::core::store::entity_timestamp_ms(entity)?;
    verify_auth_chain(&chain, &listed.id, Some(timestamp)).ok()?;
    Some((listed.chain.clone(), deployer.as_str().to_string()))
}

/// Every scene `{source}/world/{name}/scenes` lists, newest first by publication (it pages
/// oldest first by `createdAt`). Any refusal, a page without its `scenes`, a repeated id or a
/// count other than the listing's `total` is an error.
pub(super) async fn listed_scenes(
    client: &crate::http::Outbound,
    source: &str,
    name: &str,
) -> Result<Vec<ListedScene>> {
    let listing = url::Url::parse(&world_url(source, name, "scenes")?)?;
    let mut scenes: Vec<ListedScene> = Vec::new();
    let mut offset = 0usize;
    let mut total;
    loop {
        let mut url = listing.clone();
        url.query_pairs_mut()
            .append_pair("limit", &SCENES_PAGE.to_string())
            .append_pair("offset", &offset.to_string());
        let resp = client
            .get(url.as_str())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET {url} answered {status}");
        }
        let page: Value =
            serde_json::from_slice(&read_body_capped(resp, MAX_SCENES_PAGE_BYTES).await?)
                .with_context(|| format!("parsing {url}"))?;
        total = page["total"]
            .as_u64()
            .with_context(|| format!("{url} answered no total"))? as usize;
        if total > MAX_LISTED_SCENES {
            bail!("{name} lists {total} scenes, over {MAX_LISTED_SCENES}");
        }
        let rows = page["scenes"]
            .as_array()
            .with_context(|| format!("{url} answered no scenes"))?;
        for row in rows {
            let id = row["entityId"]
                .as_str()
                .filter(|id| is_canonical_cid(id))
                .with_context(|| format!("{url} lists a scene without a canonical entityId"))?;
            if scenes.iter().any(|listed| listed.id == id) {
                bail!("{url} lists scene {id} twice");
            }
            scenes.push(ListedScene::from_row(id, row));
        }
        offset += rows.len();
        if rows.is_empty() || offset >= total {
            break;
        }
    }
    if scenes.len() != total {
        bail!(
            "the scene listing of {name} names {} scenes of its total {total}",
            scenes.len()
        );
    }
    scenes.reverse();
    Ok(scenes)
}

/// Whether `{source}/world/{name}/scenes?entity_id=<cid>` still lists the scene.
pub(super) async fn source_lists_scene(
    client: &crate::http::Outbound,
    source: &str,
    name: &str,
    cid: &str,
) -> Result<bool> {
    let mut url = url::Url::parse(&world_url(source, name, "scenes")?)?;
    url.query_pairs_mut()
        .append_pair("entity_id", cid)
        .append_pair("limit", "1");
    let resp = client
        .get(url.as_str())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} answered {}", resp.status());
    }
    let listing: Value =
        serde_json::from_slice(&read_body_capped(resp, MAX_SCENE_ROW_BYTES).await?)
            .with_context(|| format!("parsing {url}"))?;
    let rows = listing["scenes"]
        .as_array()
        .with_context(|| format!("{url} answered no scenes"))?;
    Ok(rows.iter().any(|row| row["entityId"] == cid))
}

/// `(entity id, baseUrl)` per `scenesUrn` entry:
/// `urn:decentraland:entity:<cid>?=&baseUrl=<url>`.
pub(super) fn scene_refs(about: &Value) -> Vec<(String, String)> {
    about["configurations"]["scenesUrn"]
        .as_array()
        .map(|urns| {
            urns.iter()
                .filter_map(|u| u.as_str())
                .map(|urn| {
                    let after = urn
                        .split_once("urn:decentraland:entity:")
                        .map(|(_, b)| b)
                        .unwrap_or(urn);
                    let (cid, query) = after.split_once('?').unwrap_or((after, ""));
                    let base = query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("baseUrl="))
                        .map(String::from)
                        .unwrap_or_default();
                    (cid.trim().to_string(), base)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The blob from the store, or fetched from `{base_url}{hash}`, verified
/// against its CID and stored, its download slot held until then. The bool
/// says whether it was fetched.
pub(super) async fn fetch_blob(
    client: &crate::http::Outbound,
    storage: &Storage,
    base_url: &str,
    hash: &str,
    downloads: Option<&Downloads>,
) -> Result<(Bytes, bool)> {
    let (bytes, fetched, slot) = fetch_checked(client, storage, base_url, hash, downloads).await?;
    if fetched {
        storage.store(hash, bytes.clone()).await?;
    }
    drop(slot);
    Ok((bytes, fetched))
}

/// [`fetch_blob`] without the store: the bytes from the store, or fetched and verified against
/// their CID under a [`Downloads`] slot, and its byte budget, when given; the caller holds the
/// slot as long as the bytes count against the bound. The bool says whether they were fetched,
/// so are not stored yet.
pub(super) async fn fetch_checked(
    client: &crate::http::Outbound,
    storage: &Storage,
    base_url: &str,
    hash: &str,
    downloads: Option<&Downloads>,
) -> Result<(Bytes, bool, Option<DownloadSlot>)> {
    if !is_canonical_cid(hash) {
        bail!("{hash:?} is not a canonical content id");
    }
    if let Some(bytes) = storage.retrieve(hash).await? {
        return Ok((bytes, false, None));
    }
    let mut slot = match downloads {
        Some(downloads) => Some(downloads.slot().await.map_err(|e| anyhow!(e))?),
        None => None,
    };
    let url = format!("{base_url}{hash}");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} answered {}", resp.status());
    }
    if let Some(slot) = &mut slot {
        let len = resp
            .content_length()
            .unwrap_or(MAX_BLOB_BYTES as u64)
            .min(MAX_BLOB_BYTES as u64);
        slot.hold(len).await.map_err(|e| anyhow!(e))?;
    }
    let bytes = read_body_capped(resp, MAX_BLOB_BYTES)
        .await
        .with_context(|| format!("reading {url}"))?;
    if !verify_hash(&bytes, hash) {
        bail!("{url}: body does not hash to {hash}");
    }
    Ok((Bytes::from(bytes), true, slot))
}
