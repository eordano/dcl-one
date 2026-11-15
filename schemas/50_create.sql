-- Catalyst v2: the creator plane (the old catalyrst-create bundle): `builder`,
-- the pre-publish draft store behind builder-api.decentraland.org (creator
-- drafts, uploads, curation state, collection publication and linked
-- publication state, the asset-packs catalogue, newsletter opt-ins), and
-- `camera_reel`, the player photo gallery behind camera-reel-service.
--
-- Two schemas in one file, not one `create` schema: `create` is a reserved
-- word (every statement would quote it), and DATABASE.md names a schema
-- after an authority and lifecycle: a draft is the creator's, ends when the
-- collection is published (the chain is the authority afterwards and the row
-- becomes its local record) and is joined only to other builder rows; a
-- photo is the player's, lives under moderation until its owner or a
-- moderator deletes it, and is joined to nothing here. No statement joins
-- the two, so nothing is gained by sharing a namespace. They share one
-- lifecycle boundary only: both are LOCAL authority behind
-- CREATE_WRITES_ENABLED (default off, 503 with Retry-After, the way
-- src/social/writes.rs gates the social writes), and both are restored with
-- the ONE blob store, where their bytes live.
--
-- Designed from the workload, not copied. Sources:
-- catalyrst/crates/catalyrst-builder/migrations 0001..0007 and
-- catalyrst/crates/catalyrst-camera-reel/migrations 20260609..20260619.
-- Not carried: `uploaded_contents.bytes` (a 20 MB bytea per upload, kept in
-- Postgres for want of a store; v2 stores the bytes in the ONE blob store by
-- hash and keeps the acceptance record here), the camera-reel `url` /
-- `thumbnail_url` columns (a copy of the API_URL setting; the wire URL
-- renders from the hash), the camera-reel per-user `(user_address,
-- is_public)` and review-status indexes and the builder `curation_status`
-- index (no statement reads them), and the
-- catalog.json file tree plus its LRU pull cache (the catalogue is a one-row
-- reference relation; pulled asset-pack files are ordinary blobs).
--
-- Conventions: wallet addresses are lowercase 0x + 40 hex text; every
-- timestamp column is `timestamp without time zone` holding UTC (the pool
-- pins TimeZone=UTC; the old migrations declared timestamptz); the current
-- time is a bind parameter wherever it decides anything (an index
-- predicate, a due-work filter, a cursor, withheld_at) and the
-- updated_at/built_at stamps are timezone('UTC', now()) in the statement;
-- every index names its query. No FK crosses a schema:
-- camera_reel.images.place_id names an explore.place row by key and
-- reconciles on read (the mirror can drop a place; the photo stays the
-- player's fact); item_contents.hash and images.*_hash name blobs in the
-- ONE store, which is never a foreign-key target.
--
-- Domain agents append ONLY inside the two `-- ==== section: ... ====`
-- blocks at the end (builder, camera_reel), after re-reading this file.

CREATE SCHEMA builder;
CREATE SCHEMA camera_reel;

-- ══ builder ═════════════════════════════════════════════════════════════════

-- ── collections ────────────────────────────────────────────────────────────

-- A creator's collection draft. `eth_address` is the owner (the signed-fetch
-- signer at creation; every write re-checks it). A standard collection is
-- published through collection_publications (contract_address and the
-- collections-v2 urn land on publication); a linked (third-party)
-- collection carries its provider urn from the draft on (third_party_id is
-- the urn minus its last segment) and is published through
-- linked_collection_publications. `publication_pending` freezes the draft
-- and its items while either publication row is in flight.
CREATE TABLE builder.collections (
    id uuid NOT NULL,
    name text NOT NULL,
    eth_address text NOT NULL,
    salt text,
    contract_address text,
    urn_suffix text,
    third_party_id text,
    is_published boolean NOT NULL DEFAULT false,
    is_approved boolean NOT NULL DEFAULT false,
    publication_pending boolean NOT NULL DEFAULT false,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT collections_pkey PRIMARY KEY (id),
    CONSTRAINT collections_contract_address_key UNIQUE (contract_address),
    CONSTRAINT collections_eth_address_lowercase_check CHECK (eth_address = lower(eth_address)),
    CONSTRAINT collections_name_check CHECK (name = btrim(name) AND name <> ''),
    CONSTRAINT collections_salt_len_check CHECK (salt IS NULL OR char_length(salt) <= 128),
    CONSTRAINT collections_third_party_urn_check
        CHECK (third_party_id IS NULL
               OR (urn_suffix IS NOT NULL AND left(urn_suffix, char_length(third_party_id) + 1) = third_party_id || ':')),
    CONSTRAINT collections_contract_published_check CHECK (contract_address IS NULL OR is_published),
    CONSTRAINT collections_pending_not_published_check CHECK (NOT (publication_pending AND is_published))
);

-- "My collections" (GET /v1/collections: eth_address = $1 AND (NOT
-- is_published OR third_party_id IS NOT NULL) ORDER BY updated_at DESC), a
-- per-owner set of tens of rows.
CREATE INDEX collections_owner_idx ON builder.collections (eth_address);

-- ── items ──────────────────────────────────────────────────────────────────

-- An item draft (wearable or emote), owned by `eth_address`, optionally in a
-- collection of the same owner. `data`, `metrics` and `mappings` are the
-- Creator Hub documents read and written as a unit; `contents` is the
-- item_contents relation. `curation_status` is the committee's tristate and
-- `is_approved` its boolean projection (declared below, moved together by
-- the curation PATCH). The seven columns after `is_published` are the
-- explorer's BuilderApiDtos fields (wire compatibility); the copied code
-- writes urn_suffix/blockchain_item_id/local_content_hash on publication
-- and never in_catalyst/total_supply/content_hash/catalyst_content_hash.
CREATE TABLE builder.items (
    id uuid NOT NULL,
    collection_id uuid,
    eth_address text NOT NULL,
    name text NOT NULL,
    description text,
    thumbnail text,
    video text,
    type text NOT NULL,
    rarity text,
    price text,
    beneficiary text,
    data jsonb NOT NULL DEFAULT '{}'::jsonb,
    metrics jsonb,
    utility text,
    mappings jsonb,
    urn_suffix text,
    blockchain_item_id text,
    is_published boolean NOT NULL DEFAULT false,
    curation_status text NOT NULL DEFAULT 'pending',
    is_approved boolean NOT NULL DEFAULT false,
    in_catalyst boolean NOT NULL DEFAULT false,
    total_supply bigint NOT NULL DEFAULT 0,
    local_content_hash text,
    content_hash text,
    catalyst_content_hash text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT items_pkey PRIMARY KEY (id),
    CONSTRAINT items_collection_id_fkey FOREIGN KEY (collection_id)
        REFERENCES builder.collections (id) ON DELETE CASCADE,
    CONSTRAINT items_eth_address_lowercase_check CHECK (eth_address = lower(eth_address)),
    CONSTRAINT items_name_check CHECK (name = btrim(name) AND name <> ''),
    CONSTRAINT items_type_check CHECK (type IN ('wearable', 'emote')),
    CONSTRAINT items_rarity_check
        CHECK (rarity IS NULL OR rarity IN ('unique', 'mythic', 'exotic', 'legendary', 'epic', 'rare', 'uncommon', 'common')),
    CONSTRAINT items_price_check CHECK (price IS NULL OR price ~ '^[0-9]{1,78}$'),
    CONSTRAINT items_beneficiary_check CHECK (beneficiary IS NULL OR beneficiary ~ '^0x[0-9a-fA-F]{40}$'),
    CONSTRAINT items_data_object_check CHECK (jsonb_typeof(data) = 'object'),
    CONSTRAINT items_metrics_object_check CHECK (metrics IS NULL OR jsonb_typeof(metrics) = 'object'),
    CONSTRAINT items_curation_status_check CHECK (curation_status IN ('pending', 'approved', 'rejected')),
    CONSTRAINT items_approved_projection_check CHECK (is_approved = (curation_status = 'approved')),
    CONSTRAINT items_published_identity_check
        CHECK (NOT is_published OR (urn_suffix IS NOT NULL AND blockchain_item_id IS NOT NULL)),
    CONSTRAINT items_total_supply_check CHECK (total_supply >= 0)
);

-- The collection's items in creation order: GET /v1/collections/{id}/items
-- (collection_id = $1 [AND name ILIKE] ORDER BY created_at LIMIT/OFFSET with
-- count(*) OVER ()), the publication snapshot (ORDER BY created_at, id), the
-- item_count per listed collection, the curation PATCH (collection_id = $1
-- AND id = ANY($2)) and the `FOR UPDATE` sweep before a publication begins.
CREATE INDEX items_collection_created_idx ON builder.items (collection_id, created_at, id);

-- "My item drafts" (GET /v1/items: eth_address = $1 AND NOT is_published
-- ... ORDER BY updated_at DESC) and the owner probe of every item write.
CREATE INDEX items_owner_updated_idx ON builder.items (eth_address, updated_at DESC);

-- ── uploads ────────────────────────────────────────────────────────────────

-- The record that this node accepted `hash` through POST
-- /v1/items/{id}/files: the bytes are in the ONE blob store (never here),
-- `size` is what the 20 MB per-request cap was checked against,
-- `uploaded_by` the signer whose upload first stored it (a re-upload of the
-- same bytes is ON CONFLICT DO NOTHING). Rows are never deleted: a draft may
-- still name the hash, and the blob store is immutable.
CREATE TABLE builder.uploaded_contents (
    hash text NOT NULL,
    size bigint NOT NULL,
    uploaded_by text NOT NULL,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT uploaded_contents_pkey PRIMARY KEY (hash),
    CONSTRAINT uploaded_contents_size_check CHECK (size >= 0 AND size <= 20971520),
    CONSTRAINT uploaded_contents_uploaded_by_lowercase_check CHECK (uploaded_by = lower(uploaded_by))
);

-- The item's `contents` map (file name -> content hash). The FK to
-- uploaded_contents is the old "upload item content before saving its
-- metadata" rule, declared: a draft can only name a hash this node accepted,
-- so the old snapshot's `missing_content` probe is always false here.
CREATE TABLE builder.item_contents (
    item_id uuid NOT NULL,
    file text NOT NULL,
    hash text NOT NULL,
    CONSTRAINT item_contents_pkey PRIMARY KEY (item_id, file),
    CONSTRAINT item_contents_item_id_fkey FOREIGN KEY (item_id)
        REFERENCES builder.items (id) ON DELETE CASCADE,
    CONSTRAINT item_contents_hash_fkey FOREIGN KEY (hash)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT item_contents_file_check
        CHECK (file <> '' AND char_length(file) <= 256 AND position('\' in file) = 0)
);

-- ── publication ────────────────────────────────────────────────────────────

-- One standard publication attempt per collection: the reviewed Polygon
-- factory parameters (`preparation`, keyed by its content `revision`), the
-- wallet-signing claim, the submitted transaction and the verified outcome.
-- prepared -> signing -> submitted -> published | reverted; a reverted row is
-- replaced in place by the next begin, a cancel deletes an unsubmitted row.
-- The two CHECKs are the state machine: a transaction hash exists from
-- `submitted` on, a contract address only when `published`. v2 never holds a
-- signing key: the client signs; this row records what it submitted.
CREATE TABLE builder.collection_publications (
    collection_id uuid NOT NULL,
    preparation jsonb NOT NULL,
    status text NOT NULL,
    tx_hash text,
    contract_address text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT collection_publications_pkey PRIMARY KEY (collection_id),
    CONSTRAINT collection_publications_collection_id_fkey FOREIGN KEY (collection_id)
        REFERENCES builder.collections (id) ON DELETE CASCADE,
    CONSTRAINT collection_publications_tx_hash_key UNIQUE (tx_hash),
    CONSTRAINT collection_publications_contract_address_key UNIQUE (contract_address),
    CONSTRAINT collection_publications_status_check
        CHECK (status IN ('prepared', 'signing', 'submitted', 'published', 'reverted')),
    CONSTRAINT collection_publications_tx_hash_state_check
        CHECK ((status IN ('prepared', 'signing')) = (tx_hash IS NULL)),
    CONSTRAINT collection_publications_contract_state_check
        CHECK ((status = 'published') = (contract_address IS NOT NULL)),
    CONSTRAINT collection_publications_preparation_check
        CHECK (jsonb_typeof(preparation) = 'object' AND preparation ? 'revision')
);

-- One linked (third-party) publication attempt per collection: the frozen
-- draft `snapshot` and its `preparation`, the stable nonce (`salt`) the
-- provider-slot cheque is signed over, the owner-signed `cheque`, and the
-- Foundation submission receipt (`forum_url`).
-- prepared -> signing -> authorized -> submitted; cancel only while
-- prepared. The CHECKs: a cheque exists exactly from `authorized` on, a forum
-- url only once submitted.
CREATE TABLE builder.linked_collection_publications (
    collection_id uuid NOT NULL,
    preparation jsonb NOT NULL,
    snapshot jsonb NOT NULL,
    salt text NOT NULL,
    status text NOT NULL,
    cheque jsonb,
    forum_url text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT linked_collection_publications_pkey PRIMARY KEY (collection_id),
    CONSTRAINT linked_collection_publications_collection_id_fkey FOREIGN KEY (collection_id)
        REFERENCES builder.collections (id) ON DELETE CASCADE,
    CONSTRAINT linked_collection_publications_salt_key UNIQUE (salt),
    CONSTRAINT linked_collection_publications_salt_check CHECK (salt ~ '^0x[0-9a-f]{64}$'),
    CONSTRAINT linked_collection_publications_status_check
        CHECK (status IN ('prepared', 'signing', 'authorized', 'submitted')),
    CONSTRAINT linked_collection_publications_cheque_state_check
        CHECK ((status IN ('prepared', 'signing')) = (cheque IS NULL)),
    CONSTRAINT linked_collection_publications_forum_state_check
        CHECK (forum_url IS NULL OR status = 'submitted'),
    CONSTRAINT linked_collection_publications_preparation_check
        CHECK (jsonb_typeof(preparation) = 'object' AND preparation ? 'revision')
);

-- ── asset packs ────────────────────────────────────────────────────────────

-- Reference data: the asset-packs catalogue GET /v1/assetPacks serves whole
-- (the `data` array of the builder-server envelope), written by the operator
-- command that builds it from the packs tree (the old `catalyrst-builder
-- catalog`), never by a request. The hash -> file-name map GET
-- /contents/{hash} needs for a content type derives from `packs` at load;
-- the files themselves are blobs in the ONE store, pulled through from the
-- content bucket on a miss and verified against their CID.
CREATE TABLE builder.asset_catalog (
    id integer NOT NULL DEFAULT 1,
    packs jsonb NOT NULL,
    source text NOT NULL,
    built_at timestamp without time zone NOT NULL,
    CONSTRAINT asset_catalog_pkey PRIMARY KEY (id),
    CONSTRAINT asset_catalog_singleton_check CHECK (id = 1),
    CONSTRAINT asset_catalog_packs_check CHECK (jsonb_typeof(packs) = 'array')
);

-- ── newsletter ─────────────────────────────────────────────────────────────

-- POST /v1/newsletter: the durable opt-in (the forward to the newsletter
-- SaaS is best-effort and stateless). Lowercased, trimmed, validated by the
-- write path; a repeat updates `source`.
CREATE TABLE builder.newsletter_subscriptions (
    email text NOT NULL,
    source text NOT NULL,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT newsletter_subscriptions_pkey PRIMARY KEY (email),
    CONSTRAINT newsletter_subscriptions_email_check CHECK (email = lower(btrim(email)) AND email <> '')
);

-- ══ camera_reel ═════════════════════════════════════════════════════════════

-- A player's photo: the full image and its 640x360 thumbnail are blobs in
-- the ONE store named by `image_hash`/`thumbnail_hash`. The wire URLs use
-- UUID-prefixed basenames when retained, and hashes for older rows. `metadata` is the
-- client document (`Metadata`: userName, userAddress, dateTime, realm,
-- scene, visiblePeople, placeId) returned as a unit; `place_id` is its
-- placeId projected for the place galleries and names an explore.place row
-- by key (no FK: mirrored catalogue, different lifecycle). `user_address` is
-- the verified signer (the write path refuses a metadata.userAddress that is
-- not the signer; the CHECK holds the pair together). `review_status` is the
-- moderator's: `rejected` hides the row from public galleries and metadata
-- reads, and withholds unreferenced blobs on the protected byte routes.
CREATE TABLE camera_reel.images (
    id uuid NOT NULL,
    user_address text NOT NULL,
    image_hash text NOT NULL,
    thumbnail_hash text NOT NULL,
    -- The client's basename renders UUID-prefixed image and thumbnail aliases.
    -- NULL preserves the hash URLs of rows created without a filename.
    original_filename text,
    -- Both source objects use the accepted client MIME, independently of encoding.
    -- NULL leaves older rows on their byte-sniffing fallback.
    image_content_type text CONSTRAINT images_image_content_type_check
        CHECK (image_content_type IN ('image/png', 'image/jpeg')),
    is_public boolean NOT NULL DEFAULT false,
    review_status text NOT NULL DEFAULT 'ok',
    -- Old rows have no new ranking event. New uploads set false, then publish
    -- after both blobs exist; a bounded repair pass finishes interrupted work.
    ranking_published boolean NOT NULL DEFAULT true,
    metadata jsonb NOT NULL,
    place_id text GENERATED ALWAYS AS (metadata->>'placeId') STORED,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT images_pkey PRIMARY KEY (id),
    CONSTRAINT images_user_address_lowercase_check CHECK (user_address = lower(user_address)),
    CONSTRAINT images_metadata_object_check CHECK (jsonb_typeof(metadata) = 'object'),
    CONSTRAINT images_metadata_owner_check CHECK (lower(metadata->>'userAddress') = user_address),
    CONSTRAINT images_review_status_check CHECK (review_status IN ('ok', 'flagged', 'rejected'))
);

-- The owner's gallery and count (GET /api/users/{address}[/images]:
-- user_address = $1 [AND is_public AND review_status <> 'rejected' for a
-- viewer who is not the owner] ORDER BY created_at DESC, id DESC), the
-- per-user limit count inside the upload insert, and the keyset tie-breaker
-- of the new page shape. One index instead of the old (user_address) +
-- (user_address, is_public): the per-user set is at most MAX_IMAGES_PER_USER.
CREATE INDEX images_user_created_idx ON camera_reel.images (user_address, created_at DESC, id DESC);

-- The place galleries (GET /api/places/{id}/images, POST /api/places/images:
-- place_id = $1 / = ANY($1) ORDER BY created_at DESC, id DESC), which only
-- ever read public, non-rejected rows, so the predicate is the index's.
CREATE INDEX images_place_public_created_idx ON camera_reel.images (place_id, created_at DESC, id DESC)
    WHERE is_public AND review_status <> 'rejected';

-- GET /api/files/{hash}: a blob is served only while a non-rejected row
-- names it as image or thumbnail (image_hash = $1 OR thumbnail_hash = $1,
-- bitmap OR of the two).
CREATE INDEX images_image_hash_idx ON camera_reel.images (image_hash);
CREATE INDEX images_thumbnail_hash_idx ON camera_reel.images (thumbnail_hash);
-- Only pending publications enter this repair keyset; an empty due set does
-- not scan the complete retained photo gallery once per minute.
CREATE INDEX images_ranking_pending_idx ON camera_reel.images (id) WHERE NOT ranking_published;

-- ══ Domain-agent sections: append inside the matching block only ═══════════

-- ==== section: builder ====
-- The on-chain reads (/v1/{address}/collections, /v1/{address}/items, the
-- curation console's committee and under-review listings) read the
-- marketplace subgraph readers (src/marketplace/graph.rs), not a squid copy,
-- so they add no table; the Polygon receipt verification of a submitted
-- publication reads the chain and writes collection_publications above.

-- ── projects ───────────────────────────────────────────────────────────────

-- A scene project (upstream builder-server's `projects` row): owned by
-- `eth_address`, the signed-fetch signer that first wrote it (every write
-- re-checks it: upstream's Ownable.canUpsert); `cols` x `rows` is the parcel
-- layout and `parcels` .. `entities` the statistics PUT .../manifest derives
-- from the scene. `thumbnail` names an upload (the FK). DELETE is soft
-- (`is_deleted`, as upstream): the row, its manifest and its files stay and
-- every read skips it. The owner's next PUT of the id starts a new project
-- on the row: manifest, files and packs dropped, the optional fields and
-- statistics reset, `created_at` now, `files_version` moved on. Two counters
-- version the two documents a project carries: `files_version` the
-- project_files set (PUT .../files names the version it read and is refused
-- unless it still holds) and `manifest_revision` the builder manifest (the
-- ETag of GET .../manifest, the If-Match of its PUT); each is bumped by the
-- one UPDATE that checks it, so concurrent writers serialize on the row.
CREATE TABLE builder.projects (
    id uuid NOT NULL,
    eth_address text NOT NULL,
    title text NOT NULL,
    description text NOT NULL DEFAULT '',
    scene_id uuid NOT NULL,
    cols integer NOT NULL,
    rows integer NOT NULL,
    parcels integer NOT NULL DEFAULT 0,
    transforms integer NOT NULL DEFAULT 0,
    gltf_shapes integer NOT NULL DEFAULT 0,
    nft_shapes integer NOT NULL DEFAULT 0,
    scripts integer NOT NULL DEFAULT 0,
    entities integer NOT NULL DEFAULT 0,
    thumbnail text,
    creation_coords text,
    is_public boolean NOT NULL DEFAULT false,
    is_template boolean NOT NULL DEFAULT false,
    template_status text,
    video text,
    is_deleted boolean NOT NULL DEFAULT false,
    files_version bigint NOT NULL DEFAULT 0,
    manifest_revision bigint NOT NULL DEFAULT 0,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT projects_pkey PRIMARY KEY (id),
    CONSTRAINT projects_thumbnail_fkey FOREIGN KEY (thumbnail)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT projects_eth_address_lowercase_check CHECK (eth_address = lower(eth_address)),
    CONSTRAINT projects_title_check CHECK (char_length(title) <= 512),
    CONSTRAINT projects_description_check CHECK (char_length(description) <= 4096),
    CONSTRAINT projects_layout_check CHECK (cols BETWEEN 1 AND 1000 AND rows BETWEEN 1 AND 1000),
    CONSTRAINT projects_versions_check CHECK (files_version >= 0 AND manifest_revision >= 0)
);

-- "My projects" (GET /v1/projects: eth_address = $1 AND NOT is_deleted
-- ORDER BY updated_at DESC, id; GET /v1/manifests; the creation_coords reset
-- of DELETE /v1/projects/{coords}/coords), a per-owner set of tens of rows.
CREATE INDEX projects_owner_updated_idx ON builder.projects (eth_address, updated_at DESC, id)
    WHERE NOT is_deleted;

-- The builder manifest of a project ({version, project, scene}: upstream's
-- manifest.json), read and written whole by GET/PUT .../manifest; its
-- `project` member is rendered from the projects row on read, so the two
-- never disagree. The revision lives on the project row (it survives a
-- DELETE .../manifest, so a stale If-Match never matches a later document).
CREATE TABLE builder.project_manifests (
    project_id uuid NOT NULL,
    manifest jsonb NOT NULL,
    CONSTRAINT project_manifests_pkey PRIMARY KEY (project_id),
    CONSTRAINT project_manifests_project_id_fkey FOREIGN KEY (project_id)
        REFERENCES builder.projects (id) ON DELETE CASCADE,
    CONSTRAINT project_manifests_manifest_check CHECK (jsonb_typeof(manifest) = 'object')
);

-- A project's file tree, content-addressed: `path` (relative, `/`-separated,
-- no empty, `.` or `..` segment) -> the CIDv1 of an upload (the FK: the
-- bytes are in the ONE store, accepted through PUT .../blobs/{hash}) and its
-- size, copied from uploaded_contents by the write. PUT .../files replaces
-- the whole set in one transaction behind the files_version check, so a
-- reader sees one version or the next, never a mix.
CREATE TABLE builder.project_files (
    project_id uuid NOT NULL,
    path text NOT NULL,
    hash text NOT NULL,
    size bigint NOT NULL,
    CONSTRAINT project_files_pkey PRIMARY KEY (project_id, path),
    CONSTRAINT project_files_project_id_fkey FOREIGN KEY (project_id)
        REFERENCES builder.projects (id) ON DELETE CASCADE,
    CONSTRAINT project_files_hash_fkey FOREIGN KEY (hash)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT project_files_path_check
        CHECK (char_length(path) BETWEEN 1 AND 512
               AND position('\' in path) = 0
               AND position('//' in path) = 0
               AND left(path, 1) <> '/' AND right(path, 1) <> '/'
               AND path !~ '(^|/)\.\.?(/|$)'),
    CONSTRAINT project_files_size_check CHECK (size >= 0 AND size <= 20971520)
);

-- ── project asset packs ────────────────────────────────────────────────────

-- An asset pack a project owns (upstream's asset packs, scoped to one
-- project instead of the whole wallet): GET /v1/projects/{id}/assetPacks
-- answers the default packs (builder.asset_catalog, which every project
-- sees) followed by these. The owner is the project's; DELETE is soft.
-- `thumbnail` names an upload.
CREATE TABLE builder.asset_packs (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    title text NOT NULL,
    thumbnail text,
    is_deleted boolean NOT NULL DEFAULT false,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT asset_packs_pkey PRIMARY KEY (id),
    CONSTRAINT asset_packs_project_id_fkey FOREIGN KEY (project_id)
        REFERENCES builder.projects (id) ON DELETE CASCADE,
    CONSTRAINT asset_packs_thumbnail_fkey FOREIGN KEY (thumbnail)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT asset_packs_title_check CHECK (char_length(title) BETWEEN 3 AND 20)
);

-- A project's packs, in creation order (GET .../assetPacks; the 10-pack cap
-- of PUT counts them).
CREATE INDEX asset_packs_project_idx ON builder.asset_packs (project_id, created_at, id);

-- An asset of a project pack, upstream's shape: `model` names the main file
-- of `contents` (asset_contents), `thumbnail` an upload, `metrics`,
-- `parameters` and `actions` the documents the editor reads as a unit.
-- `ordinal` keeps the order the owner's PUT listed them in; a PUT replaces
-- the pack's whole asset set.
CREATE TABLE builder.assets (
    id uuid NOT NULL,
    asset_pack_id uuid NOT NULL,
    ordinal integer NOT NULL,
    legacy_id text,
    name text NOT NULL,
    model text NOT NULL,
    script text,
    thumbnail text,
    tags text[] NOT NULL DEFAULT '{}',
    category text NOT NULL,
    metrics jsonb NOT NULL DEFAULT '{}',
    parameters jsonb NOT NULL DEFAULT '[]',
    actions jsonb NOT NULL DEFAULT '[]',
    CONSTRAINT assets_pkey PRIMARY KEY (id),
    CONSTRAINT assets_asset_pack_id_fkey FOREIGN KEY (asset_pack_id)
        REFERENCES builder.asset_packs (id) ON DELETE CASCADE,
    CONSTRAINT assets_thumbnail_fkey FOREIGN KEY (thumbnail)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT assets_name_check CHECK (char_length(name) BETWEEN 3 AND 50),
    CONSTRAINT assets_ordinal_check CHECK (ordinal >= 0)
);

-- A pack's assets in order (every pack projection).
CREATE INDEX assets_pack_ordinal_idx ON builder.assets (asset_pack_id, ordinal);

-- An asset's `contents` map (file name -> upload), as item_contents.
CREATE TABLE builder.asset_contents (
    asset_id uuid NOT NULL,
    file text NOT NULL,
    hash text NOT NULL,
    CONSTRAINT asset_contents_pkey PRIMARY KEY (asset_id, file),
    CONSTRAINT asset_contents_asset_id_fkey FOREIGN KEY (asset_id)
        REFERENCES builder.assets (id) ON DELETE CASCADE,
    CONSTRAINT asset_contents_hash_fkey FOREIGN KEY (hash)
        REFERENCES builder.uploaded_contents (hash),
    CONSTRAINT asset_contents_file_check
        CHECK (file <> '' AND char_length(file) <= 256 AND position('\' in file) = 0)
);
-- ==== end section: builder ====

-- ==== section: camera_reel ====
-- World-name galleries (`name.dcl.eth`) resolve to place ids through
-- explore.place_indexed (lower(world_name) = lower($1), without the world's
-- catalogue row `id = lower(world_name)`), the old HTTP hop to the places
-- API made a local read; no cache table.
--
-- A blob this node withholds everywhere because the camera reel's moderation
-- said so: the image and thumbnail hashes of a rejected row (`reason` =
-- 'rejected'; released when the moderator sets the row back to ok/flagged)
-- and of a deleted row (`reason` = 'deleted'; the old service unlinked those
-- bytes from its private tree, the ONE store keeps them). The reel's own
-- route already answers 404 from `images.review_status`; this relation is
-- what makes the verdict hold on the content server's `/contents/{hash}`
-- (root and worlds nest) and the builder's: the process loads it into the
-- shared in-memory `content::Denylist` at boot and on every write. A hash is
-- withheld only while no non-rejected `images` row and no
-- `content.content_files` / `builder.uploaded_contents` row names it (an
-- owner's delete must not hide a deployed file that happens to be the same
-- bytes); world scene files are named inside `worlds.world_scenes.entity`
-- and are not checked. `image_id` is the
-- row whose verdict wrote it, kept as a fact (no FK: a deleted row is gone).
CREATE TABLE camera_reel.withheld_blobs (
    hash text NOT NULL,
    image_id uuid NOT NULL,
    reason text NOT NULL,
    withheld_at timestamp without time zone NOT NULL,
    CONSTRAINT withheld_blobs_pkey PRIMARY KEY (hash),
    CONSTRAINT withheld_blobs_reason_check CHECK (reason IN ('rejected', 'deleted'))
);

-- The collection items worn in a photo, as `<contract>-<itemId>` lowercase
-- (the marketplace's item identity: the per-NFT token id of a
-- collections-v2 URN is dropped, so every copy of an item matches the one
-- item a shopper looks at). Derived from `metadata.visiblePeople[*]
-- .wearables[*]`; a document without those arrays (an older shape, a photo
-- of an empty scene) yields an empty array rather than failing the index
-- build. IMMUTABLE so it can be indexed; rebuild rule = the expression.
CREATE FUNCTION camera_reel.image_wearable_items(metadata jsonb) RETURNS text[]
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT coalesce(
           array_agg(DISTINCT lower(split_part(wearable, ':', 5) || '-' || split_part(wearable, ':', 6))),
           ARRAY[]::text[])
  FROM jsonb_array_elements(CASE WHEN jsonb_typeof(metadata -> 'visiblePeople') = 'array'
                                 THEN metadata -> 'visiblePeople' ELSE '[]'::jsonb END) AS person,
       jsonb_array_elements_text(CASE WHEN jsonb_typeof(person -> 'wearables') = 'array'
                                      THEN person -> 'wearables' ELSE '[]'::jsonb END) AS wearable
  WHERE wearable LIKE 'urn:decentraland:%:collections-v2:%'
$$;

-- GET /api/wearables/{item}/images: public photos of people wearing an item
-- (image_wearable_items(metadata) && ARRAY[$1] AND is_public AND
-- review_status <> 'rejected' ORDER BY created_at DESC, id DESC), which only
-- ever reads public, non-rejected rows, so the predicate is the index's.
CREATE INDEX images_wearable_items_idx ON camera_reel.images
    USING gin (camera_reel.image_wearable_items(metadata))
    WHERE is_public AND review_status <> 'rejected';

-- The photo events camera-reel-service published to SNS, recorded for
-- in-process consumers instead (append-only outbox; camera_reel::events).
-- Workload: one insert per finalised upload (`photo-taken`, in the
-- transaction that sets images.ranking_published) and one per visibility
-- change (`photo-privacy-changed`, in the statement that changes it), so
-- writes follow the upload rate. Reads: each consumer pages `id > $cursor
-- ORDER BY id LIMIT n` (the primary key) and keeps its cursor in its own
-- schema; the outbox holds no consumer state and rows are never updated.
-- Retention: the fleet-wide `camera-photo-events-retention` job deletes rows
-- older than 30 days in bounded batches by `created_at` (the second index),
-- so the table holds about 30 days of events. `payload` is the upstream
-- event body `{type, subType, key, timestamp, metadata}`; `photo_id` names
-- an images row as a fact (no FK: the photo may be deleted later).
CREATE TABLE camera_reel.photo_events (
    id bigint GENERATED ALWAYS AS IDENTITY,
    kind text NOT NULL,
    photo_id uuid NOT NULL,
    user_address text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    CONSTRAINT photo_events_pkey PRIMARY KEY (id),
    CONSTRAINT photo_events_kind_check CHECK (kind IN ('photo-taken', 'photo-privacy-changed')),
    CONSTRAINT photo_events_user_address_lowercase_check CHECK (user_address = lower(user_address)),
    CONSTRAINT photo_events_payload_object_check CHECK (jsonb_typeof(payload) = 'object')
);
CREATE INDEX photo_events_created_at_idx ON camera_reel.photo_events (created_at);
-- ==== end section: camera_reel ====
