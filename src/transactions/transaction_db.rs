// Copyright 2021 Yiyuan Liu
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use std::{
    collections::BTreeMap,
    ffi::CString,
    fs, iter,
    marker::PhantomData,
    path::{Path, PathBuf},
    ptr,
    sync::Arc,
};

use crate::{
    column_family::UnboundColumnFamily, db::DBAccess, db_options::OptionsMustOutliveDB, ffi,
    ffi_util::to_cpath, AsColumnFamilyRef, BoundColumnFamily, ColumnFamily, ColumnFamilyDescriptor,
    DBIteratorWithThreadMode, DBRawIteratorWithThreadMode, Direction, Error, IteratorMode,
    MultiThreaded, Options, ReadOptions, SingleThreaded, SnapshotWithThreadMode, ThreadMode,
    Transaction, TransactionDBOptions, TransactionOptions, WriteBatchWithTransaction, WriteOptions,
    DB, DEFAULT_COLUMN_FAMILY_NAME,
};
use libc::{c_char, c_int, size_t};

#[cfg(not(feature = "multi-threaded-cf"))]
type DefaultThreadMode = crate::SingleThreaded;
#[cfg(feature = "multi-threaded-cf")]
type DefaultThreadMode = crate::MultiThreaded;

/// RocksDB TransactionDB.
///
/// Please read the official [guide](https://github.com/facebook/rocksdb/wiki/Transactions)
/// to learn more about RocksDB TransactionDB.
///
/// The default thread mode for [`TransactionDB`] is [`SingleThreaded`]
/// if feature `multi-threaded-cf` is not enabled.
///
/// ```
/// use rocksdb::{DB, Options, TransactionDB, SingleThreaded};
/// let path = "_path_for_transaction_db";
/// {
///     let db: TransactionDB = TransactionDB::open_default(path).unwrap();
///     db.put(b"my key", b"my value").unwrap();
///     
///     // create transaction
///     let txn = db.transaction();
///     txn.put(b"key2", b"value2");
///     txn.put(b"key3", b"value3");
///     txn.commit().unwrap();
/// }
/// let _ = DB::destroy(&Options::default(), path);
/// ```
///
/// [`SingleThreaded`]: crate::SingleThreaded
pub struct TransactionDB<T: ThreadMode = DefaultThreadMode> {
    pub(crate) inner: *mut ffi::rocksdb_transactiondb_t,
    cfs: T,
    path: PathBuf,
    _outlive: Vec<OptionsMustOutliveDB>,
}

unsafe impl<T: ThreadMode> Send for TransactionDB<T> {}
unsafe impl<T: ThreadMode> Sync for TransactionDB<T> {}

impl<T: ThreadMode> DBAccess for TransactionDB<T> {
    fn create_snapshot(&self) -> *const ffi::rocksdb_snapshot_t {
        unsafe { ffi::rocksdb_transactiondb_create_snapshot(self.inner) }
    }

    fn release_snapshot(&self, snapshot: *const ffi::rocksdb_snapshot_t) {
        unsafe {
            ffi::rocksdb_transactiondb_release_snapshot(self.inner, snapshot);
        }
    }

    fn create_iterator(&self, readopts: &ReadOptions) -> *mut ffi::rocksdb_iterator_t {
        unsafe { ffi::rocksdb_transactiondb_create_iterator(self.inner, readopts.inner) }
    }

    fn create_iterator_cf(
        &self,
        cf_handle: *mut ffi::rocksdb_column_family_handle_t,
        readopts: &ReadOptions,
    ) -> *mut ffi::rocksdb_iterator_t {
        unsafe {
            ffi::rocksdb_transactiondb_create_iterator_cf(self.inner, readopts.inner, cf_handle)
        }
    }

    fn get_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_opt(key, readopts)
    }

    fn get_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_cf_opt(cf, key, readopts)
    }
}

impl<T: ThreadMode> TransactionDB<T> {
    /// Opens a database with default options.
    pub fn open_default<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let txn_db_opts = TransactionDBOptions::default();
        Self::open(&opts, &txn_db_opts, path)
    }

    /// Opens the database with the specified options.
    pub fn open<P: AsRef<Path>>(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        path: P,
    ) -> Result<Self, Error> {
        Self::open_cf(opts, txn_db_opts, path, None::<&str>)
    }

    /// Opens a database with the given database options and column family names.
    ///
    /// Column families opened using this function will be created with default `Options`.
    pub fn open_cf<P, I, N>(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        path: P,
        cfs: I,
    ) -> Result<Self, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = N>,
        N: AsRef<str>,
    {
        let cfs = cfs
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name.as_ref(), Options::default()));

        Self::open_cf_descriptors_internal(opts, txn_db_opts, path, cfs)
    }

    /// Opens a database with the given database options and column family descriptors.
    pub fn open_cf_descriptors<P, I>(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        path: P,
        cfs: I,
    ) -> Result<Self, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = ColumnFamilyDescriptor>,
    {
        Self::open_cf_descriptors_internal(opts, txn_db_opts, path, cfs)
    }

    /// Internal implementation for opening RocksDB.
    fn open_cf_descriptors_internal<P, I>(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        path: P,
        cfs: I,
    ) -> Result<Self, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = ColumnFamilyDescriptor>,
    {
        let cfs: Vec<_> = cfs.into_iter().collect();
        let outlive = iter::once(opts.outlive.clone())
            .chain(cfs.iter().map(|cf| cf.options.outlive.clone()))
            .collect();

        let cpath = to_cpath(&path)?;

        if let Err(e) = fs::create_dir_all(&path) {
            return Err(Error::new(format!(
                "Failed to create RocksDB directory: `{:?}`.",
                e
            )));
        }

        let db: *mut ffi::rocksdb_transactiondb_t;
        let mut cf_map = BTreeMap::new();

        if cfs.is_empty() {
            db = Self::open_raw(opts, txn_db_opts, &cpath)?;
        } else {
            let mut cfs_v = cfs;
            // Always open the default column family.
            if !cfs_v.iter().any(|cf| cf.name == DEFAULT_COLUMN_FAMILY_NAME) {
                cfs_v.push(ColumnFamilyDescriptor {
                    name: String::from(DEFAULT_COLUMN_FAMILY_NAME),
                    options: Options::default(),
                });
            }
            // We need to store our CStrings in an intermediate vector
            // so that their pointers remain valid.
            let c_cfs: Vec<CString> = cfs_v
                .iter()
                .map(|cf| CString::new(cf.name.as_bytes()).unwrap())
                .collect();

            let cfnames: Vec<_> = c_cfs.iter().map(|cf| cf.as_ptr()).collect();

            // These handles will be populated by DB.
            let mut cfhandles: Vec<_> = cfs_v.iter().map(|_| ptr::null_mut()).collect();

            let cfopts: Vec<_> = cfs_v
                .iter()
                .map(|cf| cf.options.inner as *const _)
                .collect();

            db = Self::open_cf_raw(
                opts,
                txn_db_opts,
                &cpath,
                &cfs_v,
                &cfnames,
                &cfopts,
                &mut cfhandles,
            )?;

            for handle in &cfhandles {
                if handle.is_null() {
                    return Err(Error::new(
                        "Received null column family handle from DB.".to_owned(),
                    ));
                }
            }

            for (cf_desc, inner) in cfs_v.iter().zip(cfhandles) {
                cf_map.insert(cf_desc.name.clone(), inner);
            }
        }

        if db.is_null() {
            return Err(Error::new("Could not initialize database.".to_owned()));
        }

        Ok(TransactionDB {
            inner: db,
            cfs: T::new_cf_map_internal(cf_map),
            path: path.as_ref().to_path_buf(),
            _outlive: outlive,
        })
    }

    fn open_raw(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        cpath: &CString,
    ) -> Result<*mut ffi::rocksdb_transactiondb_t, Error> {
        unsafe {
            let db = ffi_try!(ffi::rocksdb_transactiondb_open(
                opts.inner,
                txn_db_opts.inner,
                cpath.as_ptr()
            ));
            Ok(db)
        }
    }

    fn open_cf_raw(
        opts: &Options,
        txn_db_opts: &TransactionDBOptions,
        cpath: &CString,
        cfs_v: &[ColumnFamilyDescriptor],
        cfnames: &[*const c_char],
        cfopts: &[*const ffi::rocksdb_options_t],
        cfhandles: &mut Vec<*mut ffi::rocksdb_column_family_handle_t>,
    ) -> Result<*mut ffi::rocksdb_transactiondb_t, Error> {
        unsafe {
            let db = ffi_try!(ffi::rocksdb_transactiondb_open_column_families(
                opts.inner,
                txn_db_opts.inner,
                cpath.as_ptr(),
                cfs_v.len() as c_int,
                cfnames.as_ptr(),
                cfopts.as_ptr(),
                cfhandles.as_mut_ptr(),
            ));
            Ok(db)
        }
    }

    fn create_inner_cf_handle(
        &self,
        name: &str,
        opts: &Options,
    ) -> Result<*mut ffi::rocksdb_column_family_handle_t, Error> {
        let cf_name = if let Ok(c) = CString::new(name.as_bytes()) {
            c
        } else {
            return Err(Error::new(
                "Failed to convert path to CString when creating cf".to_owned(),
            ));
        };
        Ok(unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_create_column_family(
                self.inner,
                opts.inner,
                cf_name.as_ptr(),
            ))
        })
    }

    pub fn list_cf<P: AsRef<Path>>(opts: &Options, path: P) -> Result<Vec<String>, Error> {
        DB::list_cf(opts, path)
    }

    pub fn destroy<P: AsRef<Path>>(opts: &Options, path: P) -> Result<(), Error> {
        DB::destroy(opts, path)
    }

    pub fn repair<P: AsRef<Path>>(opts: &Options, path: P) -> Result<(), Error> {
        DB::repair(opts, path)
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Creates a transaction with default options.
    pub fn transaction(&self) -> Transaction<Self> {
        self.transaction_opt(&WriteOptions::default(), &TransactionOptions::default())
    }

    /// Creates a transaction with options.
    pub fn transaction_opt<'a>(
        &'a self,
        write_opts: &WriteOptions,
        txn_opts: &TransactionOptions,
    ) -> Transaction<'a, Self> {
        Transaction {
            inner: unsafe {
                ffi::rocksdb_transaction_begin(
                    self.inner,
                    write_opts.inner,
                    txn_opts.inner,
                    std::ptr::null_mut(),
                )
            },
            _marker: PhantomData::default(),
        }
    }

    /// Returns the bytes associated with a key value.
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Result<Option<Vec<u8>>, Error> {
        self.get_opt(key, &ReadOptions::default())
    }

    /// Returns the bytes associated with a key value and the given column family.
    pub fn get_cf<K: AsRef<[u8]>>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_cf_opt(cf, key, &ReadOptions::default())
    }

    /// Returns the bytes associated with a key value with read options.
    pub fn get_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        unsafe {
            let mut val_len: usize = 0;
            let val_ptr = ffi_try!(ffi::rocksdb_transactiondb_get(
                self.inner,
                readopts.inner,
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                &mut val_len,
            ));
            if val_ptr.is_null() {
                Ok(None)
            } else {
                let val = Vec::from_raw_parts(val_ptr as *mut u8, val_len, val_len);
                Ok(Some(val))
            }
        }
    }

    /// Returns the bytes associated with a key value and the given column family with read options.
    pub fn get_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        unsafe {
            let mut val_len: usize = 0;
            let val_ptr = ffi_try!(ffi::rocksdb_transactiondb_get_cf(
                self.inner,
                readopts.inner,
                cf.inner(),
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                &mut val_len,
            ));
            if val_ptr.is_null() {
                Ok(None)
            } else {
                let val = Vec::from_raw_parts(val_ptr as *mut u8, val_len, val_len);
                Ok(Some(val))
            }
        }
    }

    pub fn put<K, V>(&self, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.put_opt(key, value, &WriteOptions::default())
    }

    pub fn put_cf<K, V>(&self, cf: &impl AsColumnFamilyRef, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.put_cf_opt(cf, key, value, &WriteOptions::default())
    }

    pub fn put_opt<K, V>(&self, key: K, value: V, writeopts: &WriteOptions) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_put(
                self.inner,
                writeopts.inner,
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                value.as_ref().as_ptr() as *const c_char,
                value.as_ref().len() as size_t
            ));
        }
        Ok(())
    }

    pub fn put_cf_opt<K, V>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
        value: V,
        writeopts: &WriteOptions,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_put_cf(
                self.inner,
                writeopts.inner,
                cf.inner(),
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                value.as_ref().as_ptr() as *const c_char,
                value.as_ref().len() as size_t
            ));
        }
        Ok(())
    }

    pub fn write(&self, batch: WriteBatchWithTransaction<true>) -> Result<(), Error> {
        self.write_opt(batch, &WriteOptions::default())
    }

    pub fn write_opt(
        &self,
        batch: WriteBatchWithTransaction<true>,
        writeopts: &WriteOptions,
    ) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_write(
                self.inner,
                writeopts.inner,
                batch.inner
            ));
        }
        Ok(())
    }

    pub fn merge<K, V>(&self, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.merge_opt(key, value, &WriteOptions::default())
    }

    pub fn merge_cf<K, V>(&self, cf: &impl AsColumnFamilyRef, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.merge_cf_opt(cf, key, value, &WriteOptions::default())
    }

    pub fn merge_opt<K, V>(&self, key: K, value: V, writeopts: &WriteOptions) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_merge(
                self.inner,
                writeopts.inner,
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                value.as_ref().as_ptr() as *const c_char,
                value.as_ref().len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn merge_cf_opt<K, V>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
        value: V,
        writeopts: &WriteOptions,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_merge_cf(
                self.inner,
                writeopts.inner,
                cf.inner(),
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
                value.as_ref().as_ptr() as *const c_char,
                value.as_ref().len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn delete<K: AsRef<[u8]>>(&self, key: K) -> Result<(), Error> {
        self.delete_opt(key, &WriteOptions::default())
    }

    pub fn delete_cf<K: AsRef<[u8]>>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
    ) -> Result<(), Error> {
        self.delete_cf_opt(cf, key, &WriteOptions::default())
    }

    pub fn delete_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        writeopts: &WriteOptions,
    ) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_delete(
                self.inner,
                writeopts.inner,
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
            ));
        }
        Ok(())
    }

    pub fn delete_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &impl AsColumnFamilyRef,
        key: K,
        writeopts: &WriteOptions,
    ) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_transactiondb_delete_cf(
                self.inner,
                writeopts.inner,
                cf.inner(),
                key.as_ref().as_ptr() as *const c_char,
                key.as_ref().len() as size_t,
            ));
        }
        Ok(())
    }

    pub fn iterator<'a: 'b, 'b>(
        &'a self,
        mode: IteratorMode,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        let readopts = ReadOptions::default();
        self.iterator_opt(mode, readopts)
    }

    pub fn iterator_opt<'a: 'b, 'b>(
        &'a self,
        mode: IteratorMode,
        readopts: ReadOptions,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        DBIteratorWithThreadMode::new(self, readopts, mode)
    }

    /// Opens an iterator using the provided ReadOptions.
    /// This is used when you want to iterate over a specific ColumnFamily with a modified ReadOptions
    pub fn iterator_cf_opt<'a: 'b, 'b>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
        readopts: ReadOptions,
        mode: IteratorMode,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        DBIteratorWithThreadMode::new_cf(self, cf_handle.inner(), readopts, mode)
    }

    /// Opens an iterator with `set_total_order_seek` enabled.
    /// This must be used to iterate across prefixes when `set_memtable_factory` has been called
    /// with a Hash-based implementation.
    pub fn full_iterator<'a: 'b, 'b>(
        &'a self,
        mode: IteratorMode,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
        DBIteratorWithThreadMode::new(self, opts, mode)
    }

    pub fn prefix_iterator<'a: 'b, 'b, P: AsRef<[u8]>>(
        &'a self,
        prefix: P,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        let mut opts = ReadOptions::default();
        opts.set_prefix_same_as_start(true);
        DBIteratorWithThreadMode::new(
            self,
            opts,
            IteratorMode::From(prefix.as_ref(), Direction::Forward),
        )
    }

    pub fn iterator_cf<'a: 'b, 'b>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
        mode: IteratorMode,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        let opts = ReadOptions::default();
        DBIteratorWithThreadMode::new_cf(self, cf_handle.inner(), opts, mode)
    }

    pub fn full_iterator_cf<'a: 'b, 'b>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
        mode: IteratorMode,
    ) -> DBIteratorWithThreadMode<'b, Self> {
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
        DBIteratorWithThreadMode::new_cf(self, cf_handle.inner(), opts, mode)
    }

    pub fn prefix_iterator_cf<'a, P: AsRef<[u8]>>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
        prefix: P,
    ) -> DBIteratorWithThreadMode<'a, Self> {
        let mut opts = ReadOptions::default();
        opts.set_prefix_same_as_start(true);
        DBIteratorWithThreadMode::<'a, Self>::new_cf(
            self,
            cf_handle.inner(),
            opts,
            IteratorMode::From(prefix.as_ref(), Direction::Forward),
        )
    }

    /// Opens a raw iterator over the database, using the default read options
    pub fn raw_iterator<'a: 'b, 'b>(&'a self) -> DBRawIteratorWithThreadMode<'b, Self> {
        let opts = ReadOptions::default();
        DBRawIteratorWithThreadMode::new(self, opts)
    }

    /// Opens a raw iterator over the given column family, using the default read options
    pub fn raw_iterator_cf<'a: 'b, 'b>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
    ) -> DBRawIteratorWithThreadMode<'b, Self> {
        let opts = ReadOptions::default();
        DBRawIteratorWithThreadMode::new_cf(self, cf_handle.inner(), opts)
    }

    /// Opens a raw iterator over the database, using the given read options
    pub fn raw_iterator_opt<'a: 'b, 'b>(
        &'a self,
        readopts: ReadOptions,
    ) -> DBRawIteratorWithThreadMode<'b, Self> {
        DBRawIteratorWithThreadMode::new(self, readopts)
    }

    /// Opens a raw iterator over the given column family, using the given read options
    pub fn raw_iterator_cf_opt<'a: 'b, 'b>(
        &'a self,
        cf_handle: &impl AsColumnFamilyRef,
        readopts: ReadOptions,
    ) -> DBRawIteratorWithThreadMode<'b, Self> {
        DBRawIteratorWithThreadMode::new_cf(self, cf_handle.inner(), readopts)
    }

    pub fn snapshot(&self) -> SnapshotWithThreadMode<Self> {
        SnapshotWithThreadMode::<Self>::new(self)
    }
}

impl TransactionDB<SingleThreaded> {
    /// Creates column family with given name and options.
    pub fn create_cf<N: AsRef<str>>(&mut self, name: N, opts: &Options) -> Result<(), Error> {
        let inner = self.create_inner_cf_handle(name.as_ref(), opts)?;
        self.cfs
            .cfs
            .insert(name.as_ref().to_string(), ColumnFamily { inner });
        Ok(())
    }

    /// Returns the underlying column family handle.
    pub fn cf_handle(&self, name: &str) -> Option<&ColumnFamily> {
        self.cfs.cfs.get(name)
    }
}

impl TransactionDB<MultiThreaded> {
    /// Creates column family with given name and options.
    pub fn create_cf<N: AsRef<str>>(&self, name: N, opts: &Options) -> Result<(), Error> {
        let inner = self.create_inner_cf_handle(name.as_ref(), opts)?;
        self.cfs.cfs.write().unwrap().insert(
            name.as_ref().to_string(),
            Arc::new(UnboundColumnFamily { inner }),
        );
        Ok(())
    }

    /// Returns the underlying column family handle.
    pub fn cf_handle(&self, name: &str) -> Option<Arc<BoundColumnFamily>> {
        self.cfs
            .cfs
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .map(UnboundColumnFamily::bound_column_family)
    }
}

impl<T: ThreadMode> Drop for TransactionDB<T> {
    fn drop(&mut self) {
        unsafe {
            self.cfs.drop_all_cfs_internal();
            ffi::rocksdb_transactiondb_close(self.inner);
        }
    }
}