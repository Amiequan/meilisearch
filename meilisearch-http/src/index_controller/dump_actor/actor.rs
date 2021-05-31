use std::sync::Arc;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use async_stream::stream;
use chrono::Utc;
use futures::{lock::Mutex, stream::StreamExt};
use log::{error, info};
use tokio::sync::{mpsc, oneshot, RwLock};
use update_actor::UpdateActorHandle;
use uuid_resolver::UuidResolverHandle;

use super::{DumpError, DumpInfo, DumpMsg, DumpResult, DumpStatus, DumpTask};
use crate::index_controller::{update_actor, uuid_resolver};

pub const CONCURRENT_DUMP_MSG: usize = 10;

pub struct DumpActor<UuidResolver, Update> {
    inbox: Option<mpsc::Receiver<DumpMsg>>,
    uuid_resolver: UuidResolver,
    update: Update,
    dump_path: PathBuf,
    lock: Arc<Mutex<()>>,
    dump_infos: Arc<RwLock<HashMap<String, DumpInfo>>>,
    update_db_size: u64,
    index_db_size: u64,
}

/// Generate uid from creation date
fn generate_uid() -> String {
    Utc::now().format("%Y%m%d-%H%M%S%3f").to_string()
}

impl<UuidResolver, Update> DumpActor<UuidResolver, Update>
where
    UuidResolver: UuidResolverHandle + Send + Sync + Clone + 'static,
    Update: UpdateActorHandle + Send + Sync + Clone + 'static,
{
    pub fn new(
        inbox: mpsc::Receiver<DumpMsg>,
        uuid_resolver: UuidResolver,
        update: Update,
        dump_path: impl AsRef<Path>,
        index_db_size: u64,
        update_db_size: u64,
    ) -> Self {
        let dump_infos = Arc::new(RwLock::new(HashMap::new()));
        let lock = Arc::new(Mutex::new(()));
        Self {
            inbox: Some(inbox),
            uuid_resolver,
            update,
            dump_path: dump_path.as_ref().into(),
            dump_infos,
            lock,
            index_db_size,
            update_db_size,
        }
    }

    pub async fn run(mut self) {
        info!("Started dump actor.");

        let mut inbox = self
            .inbox
            .take()
            .expect("Dump Actor must have a inbox at this point.");

        let stream = stream! {
            loop {
                match inbox.recv().await {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };

        stream
            .for_each_concurrent(Some(CONCURRENT_DUMP_MSG), |msg| self.handle_message(msg))
            .await;

        error!("Dump actor stopped.");
    }

    async fn handle_message(&self, msg: DumpMsg) {
        use DumpMsg::*;

        match msg {
            CreateDump { ret } => {
                let _ = self.handle_create_dump(ret).await;
            }
            DumpInfo { ret, uid } => {
                let _ = ret.send(self.handle_dump_info(uid).await);
            }
        }
    }

    async fn handle_create_dump(&self, ret: oneshot::Sender<DumpResult<DumpInfo>>) {
        let uid = generate_uid();
        let info = DumpInfo::new(uid.clone(), DumpStatus::InProgress);

        let _lock = match self.lock.try_lock() {
            Some(lock) => lock,
            None => {
                ret.send(Err(DumpError::DumpAlreadyRunning))
                    .expect("Dump actor is dead");
                return;
            }
        };

        self.dump_infos
            .write()
            .await
            .insert(uid.clone(), info.clone());

        ret.send(Ok(info)).expect("Dump actor is dead");

        let task = DumpTask {
            path: self.dump_path.clone(),
            uuid_resolver: self.uuid_resolver.clone(),
            update_handle: self.update.clone(),
            uid: uid.clone(),
            update_db_size: self.update_db_size,
            index_db_size: self.index_db_size,
        };

        let task_result = tokio::task::spawn(task.run()).await;

        let mut dump_infos = self.dump_infos.write().await;
        let dump_infos = dump_infos
            .get_mut(&uid)
            .expect("dump entry deleted while lock was acquired");

        match task_result {
            Ok(Ok(())) => {
                dump_infos.done();
                info!("Dump succeed");
            }
            Ok(Err(e)) => {
                dump_infos.with_error(e.to_string());
                error!("Dump failed: {}", e);
            }
            Err(_) => {
                dump_infos.with_error("Unexpected error while performing dump.".to_string());
                error!("Dump panicked. Dump status set to failed");
            }
        };
    }

    async fn handle_dump_info(&self, uid: String) -> DumpResult<DumpInfo> {
        match self.dump_infos.read().await.get(&uid) {
            Some(info) => Ok(info.clone()),
            _ => Err(DumpError::DumpDoesNotExist(uid)),
        }
    }
}
