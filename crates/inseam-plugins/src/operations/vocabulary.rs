//! The `vocabulary` owner operation (`design/vocabulary.md`,
//! observability): the vocabulary as the node holds it — rows by frequency,
//! the clusters, or one row with its gloss, aliases, cluster, and anchored
//! sources. The first thing to do after a pass is read the top of this
//! list: if it is full of industry words, the shape rule is wrong before
//! any query is run.

use std::collections::HashMap;

use inseam_kernel::address::Address;
use inseam_kernel::fragment::{FragmentId, RelationKind};
use inseam_kernel::store::{
    IndexStore, SourceId, StoredCluster, VocabularyRow, normalize_spelling,
};
use inseam_seams::SeamError;
use inseam_seams::operations::{
    ClusterView, ShownRow, VocabularyRequest, VocabularyResponse, VocabularyRowView,
};

/// Most anchored sources one `show` lists.
const SHOWN_SOURCES_MAX: u32 = 50;
/// Most member spellings a cluster view carries.
const CLUSTER_MEMBERS_SHOWN_MAX: usize = 24;

pub(super) async fn respond(
    store: &IndexStore,
    request: VocabularyRequest,
) -> Result<VocabularyResponse, SeamError> {
    let counts = Some(store.vocabulary_counts().await?);
    if let Some(spelling) = &request.show {
        return Ok(VocabularyResponse {
            counts,
            shown: show(store, spelling).await?,
            ..VocabularyResponse::default()
        });
    }
    if request.clusters {
        let mut clusters = store.clusters().await?;
        clusters.sort_by(|a, b| {
            b.document_frequency
                .cmp(&a.document_frequency)
                .then(a.id.cmp(&b.id))
        });
        let page: Vec<StoredCluster> = clusters
            .into_iter()
            .skip(usize::try_from(request.offset).unwrap_or(usize::MAX))
            .take(usize::try_from(request.limit).unwrap_or(usize::MAX))
            .collect();
        let mut views = Vec::with_capacity(page.len());
        for cluster in page {
            views.push(cluster_view(store, &cluster).await?);
        }
        return Ok(VocabularyResponse {
            counts,
            clusters: views,
            ..VocabularyResponse::default()
        });
    }
    let rows = store
        .vocabulary_rows_page(request.kind, request.limit, request.offset)
        .await?;
    Ok(VocabularyResponse {
        counts,
        rows: rows.iter().map(row_view).collect(),
        ..VocabularyResponse::default()
    })
}

/// One row by spelling: the first row that spells it, with what hangs off
/// it.
async fn show(store: &IndexStore, spelling: &str) -> Result<Option<ShownRow>, SeamError> {
    let normalized = normalize_spelling(spelling);
    let Some(row) = store
        .vocabulary_rows_spelled(&normalized)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let aliases = aliases_of(store, row.fragment).await?;
    let cluster = match row.cluster {
        Some(id) => match store.cluster(id).await? {
            Some(cluster) => Some(cluster_view(store, &cluster).await?),
            None => None,
        },
        None => None,
    };
    let sources = anchored_addresses(store, row.fragment).await?;
    Ok(Some(ShownRow {
        row: row_view(&row),
        aliases,
        cluster,
        sources,
    }))
}

async fn aliases_of(store: &IndexStore, row: FragmentId) -> Result<Vec<String>, SeamError> {
    let aliases_kind = RelationKind::new("aliases").expect("literal kind is valid");
    let alias_ids: Vec<FragmentId> = store
        .relations_touching(&[row])
        .await?
        .into_iter()
        .filter(|r| r.kind == aliases_kind && r.to == row)
        .map(|r| r.from)
        .collect();
    Ok(store
        .fragments(&alias_ids)
        .await?
        .into_iter()
        .filter_map(|f| f.text)
        .collect())
}

async fn anchored_addresses(
    store: &IndexStore,
    row: FragmentId,
) -> Result<Vec<Address>, SeamError> {
    let sources: Vec<SourceId> = store.sources_anchored_to(row, SHOWN_SOURCES_MAX).await?;
    let mut addresses = Vec::with_capacity(sources.len());
    let mut seen: HashMap<SourceId, ()> = HashMap::new();
    for source in sources {
        if seen.insert(source, ()).is_some() {
            continue;
        }
        if let Some(stored) = store.source(source).await? {
            addresses.push(stored.address);
        }
    }
    Ok(addresses)
}

async fn cluster_view(
    store: &IndexStore,
    cluster: &StoredCluster,
) -> Result<ClusterView, SeamError> {
    let members = store.cluster_members(cluster.id).await?;
    Ok(ClusterView {
        id: cluster.id.0,
        label: cluster.label.clone(),
        member_count: cluster.member_count,
        document_frequency: cluster.document_frequency,
        has_vector: cluster.vector.is_some(),
        members: members
            .iter()
            .take(CLUSTER_MEMBERS_SHOWN_MAX)
            .map(|m| m.spelling.clone())
            .collect(),
    })
}

fn row_view(row: &VocabularyRow) -> VocabularyRowView {
    VocabularyRowView {
        fragment: row.fragment,
        key: row.key.as_str().to_string(),
        kind: row.kind,
        origin: row.origin.as_str().to_string(),
        spelling: row.spelling.clone(),
        document_frequency: row.document_frequency,
        cluster: row.cluster.map(|c| c.0),
        gloss: row.gloss.clone(),
    }
}
