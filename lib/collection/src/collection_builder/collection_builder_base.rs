use std::cmp::max;
use std::fs::create_dir_all;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use num_cpus;
use parking_lot::RwLock;
use tokio::runtime;
use tokio::sync::{mpsc, Mutex};

use segment::segment_constructor::simple_segment_constructor::build_simple_segment;
use segment::types::HnswConfig;

use crate::collection::Collection;
use crate::collection_builder::optimizers_builder::build_optimizers;
use crate::collection_builder::optimizers_builder::OptimizersConfig;
use crate::collection_manager::holders::segment_holder::SegmentHolder;
use crate::config::{CollectionConfig, CollectionParams, WalConfig};
use crate::operations::types::{CollectionError, CollectionResult};
use crate::operations::CollectionUpdateOperations;
use crate::update_handler::{Optimizer, UpdateHandler};
use crate::wal::SerdeWal;

pub fn construct_collection(
    segment_holder: SegmentHolder,
    config: CollectionConfig,
    wal: SerdeWal<CollectionUpdateOperations>,
    optimizers: Arc<Vec<Arc<Optimizer>>>,
    collection_path: &Path,
) -> Collection {
    let segment_holder = Arc::new(RwLock::new(segment_holder));

    let blocking_threads = if config.optimizer_config.max_optimization_threads == 0 {
        max(num_cpus::get() - 1, 1)
    } else {
        config.optimizer_config.max_optimization_threads
    };
    let optimize_runtime = runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("optimizer-{}", id)
        })
        .max_blocking_threads(blocking_threads)
        .build()
        .unwrap();

    let locked_wal = Arc::new(Mutex::new(wal));

    let (tx, rx) = mpsc::unbounded_channel();

    let update_handler = UpdateHandler::new(
        optimizers,
        rx,
        optimize_runtime.handle().clone(),
        segment_holder.clone(),
        locked_wal.clone(),
        config.optimizer_config.flush_interval_sec,
    );

    Collection::new(
        segment_holder,
        config,
        locked_wal,
        update_handler,
        optimize_runtime,
        tx,
        collection_path.to_owned(),
    )
}

/// Creates new empty collection with given configuration
pub fn build_collection(
    collection_path: &Path,
    wal_config: &WalConfig,               // from config
    collection_params: &CollectionParams, //  from user
    optimizers_config: &OptimizersConfig,
    hnsw_config: &HnswConfig,
) -> CollectionResult<Collection> {
    let wal_path = collection_path.join("wal");

    create_dir_all(&wal_path).map_err(|err| CollectionError::ServiceError {
        error: format!("Can't create collection directory. Error: {}", err),
    })?;

    let segments_path = collection_path.join("segments");

    create_dir_all(&segments_path).map_err(|err| CollectionError::ServiceError {
        error: format!("Can't create collection directory. Error: {}", err),
    })?;

    let mut segment_holder = SegmentHolder::default();

    for _sid in 0..optimizers_config.default_segment_number {
        let segment = build_simple_segment(
            &segments_path,
            collection_params.vector_size,
            collection_params.distance,
        )?;
        segment_holder.add(segment);
    }

    let wal: SerdeWal<CollectionUpdateOperations> =
        SerdeWal::new(wal_path.to_str().unwrap(), &wal_config.into())?;

    let collection_config = CollectionConfig {
        params: collection_params.clone(),
        hnsw_config: *hnsw_config,
        optimizer_config: optimizers_config.clone(),
        wal_config: wal_config.clone(),
    };

    collection_config.save(collection_path)?;

    let optimizers = build_optimizers(
        collection_path,
        collection_params,
        optimizers_config,
        &collection_config.hnsw_config,
    );

    let collection = construct_collection(
        segment_holder,
        collection_config,
        wal,
        optimizers,
        collection_path,
    );

    Ok(collection)
}
