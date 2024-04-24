use crate::plotter::{Plotter, SectorPlottingProgress};
use crate::thread_pool_manager::PlottingThreadPoolManager;
use crate::utils::AsyncJoinOnDrop;
use async_lock::Mutex as AsyncMutex;
use async_trait::async_trait;
use event_listener_primitives::{Bag, HandlerId};
use futures::channel::mpsc;
use futures::stream::FuturesUnordered;
use futures::{select, FutureExt, Sink, SinkExt, StreamExt};
use parking_lot::Mutex;
use std::error::Error;
use std::future::pending;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use subspace_core_primitives::crypto::kzg::Kzg;
use subspace_core_primitives::{PublicKey, SectorIndex};
use subspace_erasure_coding::ErasureCoding;
use subspace_farmer_components::plotting::{
    download_sector, encode_sector, DownloadSectorOptions, EncodeSectorOptions, PlottingError,
};
use subspace_farmer_components::{FarmerProtocolInfo, PieceGetter};
use subspace_proof_of_space::Table;
use tokio::sync::Semaphore;
use tokio::task::yield_now;
use tracing::warn;

pub type HandlerFn3<A, B, C> = Arc<dyn Fn(&A, &B, &C) + Send + Sync + 'static>;
type Handler3<A, B, C> = Bag<HandlerFn3<A, B, C>, A, B, C>;

#[derive(Default, Debug)]
struct Handlers {
    plotting_progress: Handler3<PublicKey, SectorIndex, SectorPlottingProgress>,
}

/// CPU plotter
pub struct CpuPlotter<PG, PosTable>
where
    PosTable: Table,
{
    piece_getter: PG,
    downloading_semaphore: Arc<Semaphore>,
    // TODO: It is ugly that thread pool manager and table generators are both having independent
    //  mutexes, they should be combined
    plotting_thread_pool_manager: PlottingThreadPoolManager,
    table_generators: Arc<Mutex<Vec<Vec<<PosTable as Table>::Generator>>>>,
    global_mutex: Arc<AsyncMutex<()>>,
    kzg: Kzg,
    erasure_coding: ErasureCoding,
    handlers: Arc<Handlers>,
    tasks_sender: mpsc::Sender<AsyncJoinOnDrop<()>>,
    _background_tasks: AsyncJoinOnDrop<()>,
    abort_early: Arc<AtomicBool>,
}

impl<PG, PosTable> Drop for CpuPlotter<PG, PosTable>
where
    PosTable: Table,
{
    fn drop(&mut self) {
        self.abort_early.store(true, Ordering::Release);
        self.tasks_sender.close_channel();
    }
}

#[async_trait]
impl<PG, PosTable> Plotter for CpuPlotter<PG, PosTable>
where
    PG: PieceGetter + Clone + Send + Sync + 'static,
    PosTable: Table,
{
    async fn plot_sector<PS>(
        &self,
        public_key: PublicKey,
        sector_index: SectorIndex,
        farmer_protocol_info: FarmerProtocolInfo,
        pieces_in_sector: u16,
        replotting: bool,
        mut progress_sender: PS,
    ) where
        PS: Sink<SectorPlottingProgress> + Unpin + Send + 'static,
        PS::Error: Error,
    {
        let progress_updater = ProgressUpdater {
            public_key,
            sector_index,
            handlers: Arc::clone(&self.handlers),
        };

        let start = Instant::now();

        // Done outside the future below as a backpressure, ensuring that it is not possible to
        // schedule unbounded number of plotting tasks
        let downloading_permit = match Arc::clone(&self.downloading_semaphore)
            .acquire_owned()
            .await
        {
            Ok(downloading_permit) => downloading_permit,
            Err(error) => {
                warn!(%error, "Failed to acquire downloading permit");

                progress_updater
                    .update_progress_and_events(
                        &mut progress_sender,
                        SectorPlottingProgress::Error {
                            error: format!("Failed to acquire downloading permit: {error}"),
                        },
                    )
                    .await;

                return;
            }
        };

        let plotting_fut = {
            let piece_getter = self.piece_getter.clone();
            let plotting_thread_pool_manager = self.plotting_thread_pool_manager.clone();
            let table_generators = Arc::clone(&self.table_generators);
            let global_mutex = Arc::clone(&self.global_mutex);
            let kzg = self.kzg.clone();
            let erasure_coding = self.erasure_coding.clone();
            let abort_early = Arc::clone(&self.abort_early);

            async move {
                // Downloading
                let downloaded_sector = {
                    if !progress_updater
                        .update_progress_and_events(
                            &mut progress_sender,
                            SectorPlottingProgress::Downloading,
                        )
                        .await
                    {
                        return;
                    }

                    // Take mutex briefly to make sure plotting is allowed right now
                    global_mutex.lock().await;

                    let start = Instant::now();

                    let downloaded_sector_fut = download_sector(DownloadSectorOptions {
                        public_key: &public_key,
                        sector_index,
                        piece_getter: &piece_getter,
                        farmer_protocol_info,
                        kzg: &kzg,
                        pieces_in_sector,
                    });

                    let downloaded_sector = match downloaded_sector_fut.await {
                        Ok(downloaded_sector) => downloaded_sector,
                        Err(error) => {
                            warn!(%error, "Failed to download sector");

                            progress_updater
                                .update_progress_and_events(
                                    &mut progress_sender,
                                    SectorPlottingProgress::Error {
                                        error: format!("Failed to download sector: {error}"),
                                    },
                                )
                                .await;

                            return;
                        }
                    };

                    if !progress_updater
                        .update_progress_and_events(
                            &mut progress_sender,
                            SectorPlottingProgress::Downloaded(start.elapsed()),
                        )
                        .await
                    {
                        return;
                    }

                    downloaded_sector
                };

                // Plotting
                let (sector, sector_metadata, plotted_sector) = {
                    let thread_pools = plotting_thread_pool_manager.get_thread_pools().await;

                    let mut local_table_generators = table_generators.lock().pop().expect(
                        "Number of table generators is the same as number of thread pools; qed",
                    );

                    let plotting_fn = || {
                        tokio::task::block_in_place(|| {
                            let mut sector = Vec::new();
                            let mut sector_metadata = Vec::new();

                            let plotted_sector = encode_sector::<PosTable>(
                                downloaded_sector,
                                EncodeSectorOptions {
                                    sector_index,
                                    erasure_coding: &erasure_coding,
                                    pieces_in_sector,
                                    sector_output: &mut sector,
                                    sector_metadata_output: &mut sector_metadata,
                                    table_generators: &mut local_table_generators,
                                    abort_early: &abort_early,
                                    global_mutex: &global_mutex,
                                },
                            )?;

                            Ok((sector, sector_metadata, plotted_sector))
                        })
                    };

                    let thread_pool = if replotting {
                        &thread_pools.replotting
                    } else {
                        &thread_pools.plotting
                    };

                    // Give a chance to interrupt plotting if necessary
                    yield_now().await;

                    if !progress_updater
                        .update_progress_and_events(
                            &mut progress_sender,
                            SectorPlottingProgress::Encoding,
                        )
                        .await
                    {
                        return;
                    }

                    let start = Instant::now();

                    let plotting_result = thread_pool.install(plotting_fn);

                    table_generators.lock().push(local_table_generators);

                    match plotting_result {
                        Ok(plotting_result) => {
                            if !progress_updater
                                .update_progress_and_events(
                                    &mut progress_sender,
                                    SectorPlottingProgress::Encoded(start.elapsed()),
                                )
                                .await
                            {
                                return;
                            }

                            plotting_result
                        }
                        Err(PlottingError::AbortEarly) => {
                            return;
                        }
                        Err(error) => {
                            progress_updater
                                .update_progress_and_events(
                                    &mut progress_sender,
                                    SectorPlottingProgress::Error {
                                        error: format!("Failed to encode sector: {error}"),
                                    },
                                )
                                .await;

                            return;
                        }
                    }
                };

                progress_updater
                    .update_progress_and_events(
                        &mut progress_sender,
                        SectorPlottingProgress::Finished {
                            plotted_sector,
                            time: start.elapsed(),
                            sector,
                            sector_metadata,
                        },
                    )
                    .await;

                drop(downloading_permit);
            }
        };

        // Spawn a separate task such that `block_in_place` inside will not affect anything else
        let plotting_task = AsyncJoinOnDrop::new(tokio::spawn(plotting_fut), true);
        if let Err(error) = self.tasks_sender.clone().send(plotting_task).await {
            warn!(%error, "Failed to send plotting task");

            let progress = SectorPlottingProgress::Error {
                error: format!("Failed to send plotting task: {error}"),
            };

            self.handlers
                .plotting_progress
                .call_simple(&public_key, &sector_index, &progress);
        }
    }
}

impl<PG, PosTable> CpuPlotter<PG, PosTable>
where
    PG: PieceGetter + Clone + Send + Sync + 'static,
    PosTable: Table,
{
    /// Create new instance
    pub fn new(
        piece_getter: PG,
        downloading_semaphore: Arc<Semaphore>,
        plotting_thread_pool_manager: PlottingThreadPoolManager,
        record_encoding_concurrency: NonZeroUsize,
        global_mutex: Arc<AsyncMutex<()>>,
        kzg: Kzg,
        erasure_coding: ErasureCoding,
    ) -> Self {
        let table_generators = (0..plotting_thread_pool_manager.thread_pool_pairs().get())
            .map(|_| {
                (0..record_encoding_concurrency.get())
                    .map(|_| PosTable::generator())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let (tasks_sender, mut tasks_receiver) = mpsc::channel(1);

        // Basically runs plotting tasks in the background and allows to abort on drop
        let background_tasks = AsyncJoinOnDrop::new(
            tokio::spawn(async move {
                let background_tasks = FuturesUnordered::new();
                let mut background_tasks = pin!(background_tasks);
                // Just so that `FuturesUnordered` will never end
                background_tasks.push(AsyncJoinOnDrop::new(tokio::spawn(pending::<()>()), true));

                loop {
                    select! {
                        maybe_background_task = tasks_receiver.next().fuse() => {
                            if let Some(background_task) = maybe_background_task {
                                background_tasks.push(background_task);
                            } else {
                                break;
                            }
                        },
                        _ = background_tasks.select_next_some() => {
                            // Nothing to do
                        }
                    }
                }
            }),
            true,
        );

        let abort_early = Arc::new(AtomicBool::new(false));

        Self {
            piece_getter,
            downloading_semaphore,
            plotting_thread_pool_manager,
            table_generators: Arc::new(Mutex::new(table_generators)),
            global_mutex,
            kzg,
            erasure_coding,
            handlers: Arc::default(),
            tasks_sender,
            _background_tasks: background_tasks,
            abort_early,
        }
    }

    /// Subscribe to plotting progress notifications
    pub fn on_plotting_progress(
        &self,
        callback: HandlerFn3<PublicKey, SectorIndex, SectorPlottingProgress>,
    ) -> HandlerId {
        self.handlers.plotting_progress.add(callback)
    }
}

struct ProgressUpdater {
    public_key: PublicKey,
    sector_index: SectorIndex,
    handlers: Arc<Handlers>,
}

impl ProgressUpdater {
    /// Returns `true` on success and `false` if progress receiver channel is gone
    async fn update_progress_and_events<PS>(
        &self,
        progress_sender: &mut PS,
        progress: SectorPlottingProgress,
    ) -> bool
    where
        PS: Sink<SectorPlottingProgress> + Unpin,
        PS::Error: Error,
    {
        self.handlers.plotting_progress.call_simple(
            &self.public_key,
            &self.sector_index,
            &progress,
        );

        if let Err(error) = progress_sender.send(progress).await {
            warn!(%error, "Failed to send error progress update");

            false
        } else {
            true
        }
    }
}