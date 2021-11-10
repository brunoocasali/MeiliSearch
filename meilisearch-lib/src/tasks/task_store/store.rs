#[allow(clippy::upper_case_acronyms)]
type BEU64 = heed::zerocopy::U64<heed::byteorder::BE>;

const UID_TASK_IDS: &str = "uid_task_id";
const TASKS: &str = "tasks";

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::convert::TryInto;
use std::path::Path;
use std::result::Result as StdResult;

use heed::types::{ByteSlice, OwnedType, SerdeJson, Unit};
use heed::{BytesDecode, BytesEncode, Database, Env, EnvOpenOptions, RoTxn, RwTxn};

use crate::tasks::task::{Task, TaskId};

use super::super::Result;

use super::TaskFilter;

enum IndexUidTaskIdCodec {}

impl<'a> BytesEncode<'a> for IndexUidTaskIdCodec {
    type EItem = (&'a str, TaskId);

    fn bytes_encode((s, id): &'a Self::EItem) -> Option<Cow<'a, [u8]>> {
        let size = s.len() + std::mem::size_of::<TaskId>() + 1;
        if size > 512 {
            return None;
        }
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(s.as_bytes());
        // null terminate the string
        b.push(0);
        b.extend_from_slice(&id.to_be_bytes());
        Some(Cow::Owned(b))
    }
}

impl<'a> BytesDecode<'a> for IndexUidTaskIdCodec {
    type DItem = (&'a str, TaskId);

    fn bytes_decode(bytes: &'a [u8]) -> Option<Self::DItem> {
        let str_end = bytes.iter().position(|&it| it == 0)?;
        let str_bytes = &bytes[..str_end];
        let str = std::str::from_utf8(str_bytes).ok()?;
        let id = TaskId::from_be_bytes(bytes[str_end + 1..].try_into().ok()?);
        Some((str, id))
    }
}

pub struct Store {
    env: Env,
    uids_task_ids: Database<IndexUidTaskIdCodec, Unit>,
    tasks: Database<OwnedType<BEU64>, SerdeJson<Task>>,
}

impl Store {
    pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
        let mut options = EnvOpenOptions::new();
        options.map_size(size);
        options.max_dbs(1000);
        let env = options.open(path)?;

        let uids_task_ids = env.create_database(Some(UID_TASK_IDS))?;
        let tasks = env.create_database(Some(TASKS))?;

        Ok(Self {
            env,
            uids_task_ids,
            tasks,
        })
    }

    pub fn wtxn(&self) -> Result<RwTxn> {
        Ok(self.env.write_txn()?)
    }

    pub fn rtxn(&self) -> Result<RoTxn> {
        Ok(self.env.read_txn()?)
    }

    pub fn next_task_id(&self, txn: &mut RwTxn) -> Result<TaskId> {
        let id = self
            .tasks
            .lazily_decode_data()
            .last(txn)?
            .map(|(id, _)| id.get() + 1)
            .unwrap_or(0);
        Ok(id)
    }

    pub fn put(&self, txn: &mut RwTxn, task: &Task) -> Result<()> {
        self.tasks.put(txn, &BEU64::new(task.id), task)?;
        self.uids_task_ids
            .put(txn, &(&task.index_uid, task.id), &())?;

        Ok(())
    }

    pub fn get(&self, txn: &RoTxn, id: TaskId) -> Result<Option<Task>> {
        let task = self.tasks.get(txn, &BEU64::new(id))?;
        Ok(task)
    }

    pub fn task_count(&self, txn: &RoTxn) -> Result<usize> {
        Ok(self.tasks.len(txn)?)
    }

    pub fn list_tasks<'a>(
        &self,
        txn: &'a RoTxn,
        from: Option<TaskId>,
        filter: Option<TaskFilter>,
        limit: Option<usize>,
        until: Option<TaskId>,
    ) -> Result<Vec<Task>> {
        let iter: Box<dyn Iterator<Item = StdResult<_, heed::Error>>> = match filter {
            Some(filter) => {
                let iter = self
                    .compute_candidates(txn, filter, until)?
                    .into_iter()
                    .skip_while(|&id| from.map(|from| from == id).unwrap_or_default())
                    .filter_map(|id| self.tasks.get(txn, &BEU64::new(id)).transpose());

                Box::new(iter)
            }
            None => match from {
                Some(id) => Box::new(
                    self.tasks
                        .rev_range(txn, &(..BEU64::new(id)))?
                        .map(|r| r.map(|(_, t)| t)),
                ),
                None => Box::new(self.tasks.rev_iter(txn)?.map(|r| r.map(|(_, t)| t))),
            },
        };

        // Collect 'limit' task if it exists or all of them.
        let tasks = iter
            .take(limit.unwrap_or(usize::MAX))
            .try_fold::<_, _, StdResult<_, heed::Error>>(Vec::new(), |mut v, task| {
                v.push(task?);
                Ok(v)
            })?;

        Ok(tasks)
    }

    fn compute_candidates(
        &self,
        txn: &heed::RoTxn,
        filter: TaskFilter,
        until: Option<TaskId>,
    ) -> Result<BTreeSet<TaskId>> {
        let mut candidates = BTreeSet::new();
        if let Some(indexes) = filter.indexes {
            for index in indexes {
                // We need to prefix search the null terminated string to make sure that we only
                // get exact matches for the index, and not other uids that would share the same
                // prefix, i.e test and test1.
                let mut index_uid = index.as_bytes().to_vec();
                index_uid.push(0);
                self.uids_task_ids
                    .remap_key_type::<ByteSlice>()
                    .rev_prefix_iter(txn, &index_uid)?
                    .map(|entry| -> StdResult<_, heed::Error> {
                        let (key, _) = entry?;
                        let (_, id) =
                            IndexUidTaskIdCodec::bytes_decode(key).ok_or(heed::Error::Decoding)?;
                        Ok(id)
                    })
                    .take_while(|entry| {
                        entry
                            .as_ref()
                            .ok()
                            .zip(until)
                            .map(|(key, until)| *key > until)
                            .unwrap_or(true) // if we encountered an error we returns true to collect it later
                    })
                    .try_for_each::<_, StdResult<(), heed::Error>>(|id| {
                        Ok(drop(candidates.insert(id?)))
                    })?;
            }
        }

        Ok(candidates)
    }
}

#[cfg(test)]
pub mod test {
    use std::collections::{HashMap, HashSet};

    use nelson::Mocker;
    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;

    use super::*;

    pub enum MockStore {
        Real(Store),
        Fake(Mocker),
    }

    impl MockStore {
        pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
            Ok(Self::Real(Store::new(path, size)?))
        }

        pub fn wtxn(&self) -> Result<RwTxn> {
            match self {
                MockStore::Real(index) => index.wtxn(),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn rtxn(&self) -> Result<RoTxn> {
            match self {
                MockStore::Real(index) => index.rtxn(),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn next_task_id(&self, txn: &mut RwTxn) -> Result<TaskId> {
            match self {
                MockStore::Real(index) => index.next_task_id(txn),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn put(&self, txn: &mut RwTxn, task: &Task) -> Result<()> {
            match self {
                MockStore::Real(index) => index.put(txn, task),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn get(&self, txn: &RoTxn, id: TaskId) -> Result<Option<Task>> {
            match self {
                MockStore::Real(index) => index.get(txn, id),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn task_count(&self, txn: &RoTxn) -> Result<usize> {
            match self {
                MockStore::Real(index) => index.task_count(txn),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn list_tasks<'a>(
            &self,
            txn: &'a RoTxn,
            from: Option<TaskId>,
            filter: Option<TaskFilter>,
            limit: Option<usize>,
            until: Option<TaskId>,
        ) -> Result<Vec<Task>> {
            match self {
                MockStore::Real(index) => index.list_tasks(txn, from, filter, limit, until),
                MockStore::Fake(_) => todo!(),
            }
        }
    }

    #[quickcheck]
    fn put_retrieve_task(tasks: Vec<Task>) -> TestResult {
        // if two task have the same id, we discard the test.
        if tasks.is_empty()
            || tasks.iter().map(|t| t.id).collect::<HashSet<_>>().len() != tasks.len()
        {
            return TestResult::discard();
        }
        let tmp = tempfile::tempdir().unwrap();

        let store = Store::new(tmp.path(), 4096 * 10000000).unwrap();

        let mut txn = store.wtxn().unwrap();

        for task in tasks.iter() {
            store.put(&mut txn, task).unwrap();
        }

        txn.commit().unwrap();

        let txn = store.rtxn().unwrap();

        assert_eq!(store.task_count(&txn).unwrap(), tasks.len());

        for task in tasks {
            let found_task = store.get(&txn, task.id).unwrap().unwrap();
            assert_eq!(found_task, task);
        }

        TestResult::passed()
    }

    #[quickcheck]
    fn list_updates(tasks: Vec<Task>) -> TestResult {
        // if two task have the same id, we discard the test.
        if tasks.is_empty()
            || tasks.iter().map(|t| t.id).collect::<HashSet<_>>().len() != tasks.len()
        {
            return TestResult::discard();
        }

        let tmp = tempfile::tempdir().unwrap();

        let store = Store::new(tmp.path(), 4096 * 100000).unwrap();

        let mut txn = store.wtxn().unwrap();

        for task in tasks.iter() {
            store.put(&mut txn, task).unwrap();
        }

        txn.commit().unwrap();

        let txn = store.rtxn().unwrap();
        let validator = tasks
            .into_iter()
            .map(|t| (t.id, t))
            .collect::<HashMap<_, _>>();

        assert_eq!(store.task_count(&txn).unwrap(), validator.len());

        let iter = store
            .list_tasks(&txn, None, None, None)
            .unwrap()
            .into_iter()
            .map(|t| (t.id, t))
            .collect::<HashMap<_, _>>();

        assert_eq!(iter, validator);

        let randid = validator.values().next().unwrap().id;

        store
            .list_tasks(&txn, Some(randid), None, None)
            .unwrap()
            .into_iter()
            .for_each(|t| assert!(t.id < randid, "id: {}, randid: {}", t.id, randid));

        TestResult::passed()
    }

    #[quickcheck]
    fn list_updates_filter(tasks: Vec<Task>) -> TestResult {
        // if two task have the same id, we discard the test.
        if tasks.is_empty()
            || tasks.iter().map(|t| t.id).collect::<HashSet<_>>().len() != tasks.len()
        {
            return TestResult::discard();
        }

        let index_to_filter = tasks.first().unwrap().index_uid.clone();

        let tmp = tempfile::tempdir().unwrap();

        let store = Store::new(tmp.path(), 4096 * 100000).unwrap();

        let mut txn = store.wtxn().unwrap();

        for task in tasks.iter() {
            store.put(&mut txn, task).unwrap();
        }

        txn.commit().unwrap();

        let txn = store.rtxn().unwrap();
        let validator = tasks
            .into_iter()
            .map(|t| (t.id, t))
            .collect::<HashMap<_, _>>();

        assert_eq!(store.task_count(&txn).unwrap(), validator.len());

        let mut filter = TaskFilter::default();
        filter.filter_index(index_to_filter.clone());

        let tasks = store.list_tasks(&txn, None, Some(filter), None).unwrap();

        assert!(!tasks.is_empty());
        tasks.into_iter().for_each(|task| {
            assert_eq!(task.index_uid, index_to_filter);
        });
        TestResult::passed()
    }

    #[test]
    fn test_filter_same_index_prefix() {
        let tmp = tempfile::tempdir().unwrap();

        let store = Store::new(tmp.path(), 4096 * 100000).unwrap();

        let mut gen = Gen::new(30);

        // task1 and 2 share the same index_uid prefix
        let mut task_1 = Task::arbitrary(&mut gen);
        task_1.index_uid = "test".into();

        let mut task_2 = Task::arbitrary(&mut gen);
        task_2.index_uid = "test1".into();

        let mut txn = store.wtxn().unwrap();
        store.put(&mut txn, &task_1).unwrap();
        store.put(&mut txn, &task_2).unwrap();

        let mut filter = TaskFilter::default();
        filter.filter_index("test".into());

        let tasks = store.list_tasks(&txn, None, Some(filter), None).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks.first().unwrap().index_uid, "test");
    }
}
