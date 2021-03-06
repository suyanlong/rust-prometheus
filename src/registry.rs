// Copyright 2014 The Prometheus Authors
// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use errors::{Error, Result};
use metrics::Collector;

use proto;
use spin::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::collections::btree_map::Entry as BEntry;
use std::collections::hash_map::Entry as HEntry;
use std::iter::FromIterator;
use std::sync::Arc;

struct RegistryCore {
    pub colloctors_by_id: HashMap<u64, Box<Collector>>,
    pub dim_hashes_by_name: HashMap<String, u64>,
    pub desc_ids: HashSet<u64>,
}

impl RegistryCore {
    fn register(&mut self, c: Box<Collector>) -> Result<()> {
        let mut desc_id_set = HashSet::new();
        let mut collector_id: u64 = 0;

        for desc in c.desc() {
            // Is the desc_id unique?
            // (In other words: Is the fqName + constLabel combination unique?)
            if self.desc_ids.contains(&desc.id) {
                return Err(Error::Msg(format!(
                    "descriptor {:?} already exists with the same \
                     fully-qualified name and const label values",
                    desc
                )));
            }

            if let Some(hash) = self.dim_hashes_by_name.get(&desc.fq_name) {
                if *hash != desc.dim_hash {
                    return Err(Error::Msg(format!(
                        "a previously registered descriptor with the \
                         same fully-qualified name as {:?} has \
                         different label names or a different help \
                         string",
                        desc
                    )));
                }
            }

            self.dim_hashes_by_name
                .insert(desc.fq_name.clone(), desc.dim_hash);

            // If it is not a duplicate desc in this collector, add it to
            // the collector_id.
            if desc_id_set.insert(desc.id) {
                // The set did not have this value present, true is returned.
                collector_id = collector_id.wrapping_add(desc.id);
            } else {
                // The set did have this value present, false is returned.
                //
                // TODO: Should we allow duplicate descs within the same collector?
                return Err(Error::Msg(format!(
                    "a duplicate descriptor within the same \
                     collector the same fully-qualified name: {:?}",
                    desc.fq_name
                )));
            }
        }

        match self.colloctors_by_id.entry(collector_id) {
            HEntry::Vacant(vc) => {
                self.desc_ids.extend(desc_id_set);
                vc.insert(c);
                Ok(())
            }
            HEntry::Occupied(_) => Err(Error::AlreadyReg),
        }
    }

    fn unregister(&mut self, c: Box<Collector>) -> Result<()> {
        let mut id_set = Vec::new();
        let mut collector_id = 0;
        for desc in c.desc() {
            if id_set.iter().find(|id| **id == desc.id).is_none() {
                id_set.push(desc.id);
                collector_id += desc.id;
            }
        }

        if self.colloctors_by_id.remove(&collector_id).is_none() {
            return Err(Error::Msg(
                format!("collector {:?} is not registered", c.desc()),
            ));
        }

        // dim_hashes_by_name is left untouched as those must be consistent
        // throughout the lifetime of a program.
        Ok(())
    }

    fn gather(&self) -> Vec<proto::MetricFamily> {
        let mut mf_by_name = BTreeMap::new();

        for c in self.colloctors_by_id.values() {
            let mfs = c.collect();
            for mut mf in mfs {
                let name = mf.get_name().to_owned();

                match mf_by_name.entry(name) {
                    BEntry::Vacant(entry) => {
                        entry.insert(mf);
                    }
                    BEntry::Occupied(mut entry) => {
                        let existent_mf = entry.get_mut();
                        let existent_metrics = existent_mf.mut_metric();

                        // TODO: check type.
                        // TODO: check consistency.
                        for metric in mf.take_metric().into_iter() {
                            existent_metrics.push(metric);
                        }
                    }
                }
            }
        }

        // TODO: metric_family injection hook.

        // Now that MetricFamilies are all set, sort their Metrics
        // lexicographically by their label values.
        for mf in mf_by_name.values_mut() {
            mf.mut_metric().sort_by(|m1, m2| {
                let lps1 = m1.get_label();
                let lps2 = m2.get_label();

                if lps1.len() != lps2.len() {
                    // This should not happen. The metrics are
                    // inconsistent. However, we have to deal with the fact, as
                    // people might use custom collectors or metric family injection
                    // to create inconsistent metrics. So let's simply compare the
                    // number of labels in this case. That will still yield
                    // reproducible sorting.
                    return lps1.len().cmp(&lps2.len());
                }

                for (lp1, lp2) in lps1.iter().zip(lps2.iter()) {
                    if lp1.get_value() != lp2.get_value() {
                        return lp1.get_value().cmp(lp2.get_value());
                    }
                }

                // We should never arrive here. Multiple metrics with the same
                // label set in the same scrape will lead to undefined ingestion
                // behavior. However, as above, we have to provide stable sorting
                // here, even for inconsistent metrics. So sort equal metrics
                // by their timestamp, with missing timestamps (implying "now")
                // coming last.
                m1.get_timestamp_ms().cmp(&m2.get_timestamp_ms())
            });
        }

        // Write out MetricFamilies sorted by their name.
        let kvs = Vec::from_iter(mf_by_name.into_iter());
        kvs.into_iter().map(|(_, m)| m).collect()
    }
}

/// `Registry` registers Prometheus collectors, collects their metrics, and gathers
/// them into `MetricFamilies` for exposition.
#[derive(Clone)]
pub struct Registry {
    r: Arc<RwLock<RegistryCore>>,
}

impl Default for Registry {
    fn default() -> Registry {
        let r = RegistryCore {
            colloctors_by_id: HashMap::new(),
            dim_hashes_by_name: HashMap::new(),
            desc_ids: HashSet::new(),
        };

        Registry {
            r: Arc::new(RwLock::new(r)),
        }
    }
}

impl Registry {
    /// `new` creates a Registry.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// `register` registers a new Collector to be included in metrics
    /// collection. It returns an error if the descriptors provided by the
    /// Collector are invalid or if they — in combination with descriptors of
    /// already registered Collectors — do not fulfill the consistency and
    /// uniqueness criteria described in the documentation of `Desc`.
    ///
    /// If the provided Collector is equal to a Collector already registered
    /// (which includes the case of re-registering the same Collector), the
    /// AlreadyReg error returns.
    pub fn register(&self, c: Box<Collector>) -> Result<()> {
        self.r.write().register(c)
    }

    /// `unregister` unregisters the Collector that equals the Collector passed
    /// in as an argument.  (Two Collectors are considered equal if their
    /// Describe method yields the same set of descriptors.) The function
    /// returns error when the Collector is not registered.
    pub fn unregister(&self, c: Box<Collector>) -> Result<()> {
        self.r.write().unregister(c)
    }

    /// `gather` calls the Collect method of the registered Collectors and then
    /// gathers the collected metrics into a lexicographically sorted slice
    /// of MetricFamily protobufs.
    pub fn gather(&self) -> Vec<proto::MetricFamily> {
        self.r.read().gather()
    }
}

cfg_if! {
    if #[cfg(all(feature = "process", target_os="linux"))] {
        fn register_default_process_collector(reg: &Registry) -> Result<()> {
            use process_collector::ProcessCollector;

            let pc = ProcessCollector::for_self();
            reg.register(Box::new(pc))
        }
    } else {
        fn register_default_process_collector(_: &Registry) -> Result<()> {
            Ok(())
        }
    }
}

// Default registry for rust-prometheus.
lazy_static! {
    static ref DEFAULT_REGISTRY: Registry = {
        let reg = Registry::default();

        // Register a default process collector.
        register_default_process_collector(&reg).unwrap();

        reg
    };
}

/// `register` registers a new Collector to be included in metrics collection. It
/// returns an error if the descriptors provided by the Collector are invalid or
/// if they - in combination with descriptors of already registered Collectors -
/// do not fulfill the consistency and uniqueness criteria described in the Desc
/// documentation.
pub fn register(c: Box<Collector>) -> Result<()> {
    DEFAULT_REGISTRY.register(c)
}

/// `unregister` unregisters the Collector that equals the Collector passed in as
/// an argument. (Two Collectors are considered equal if their Describe method
/// yields the same set of descriptors.) The function returns an error if a
/// Collector was not registered.
pub fn unregister(c: Box<Collector>) -> Result<()> {
    DEFAULT_REGISTRY.unregister(c)
}

/// `gather` returns all `MetricFamily` of `DEFAULT_REGISTRY`.
pub fn gather() -> Vec<proto::MetricFamily> {
    DEFAULT_REGISTRY.gather()
}

#[cfg(test)]
mod tests {

    use super::*;

    use counter::{Counter, CounterVec};
    use desc::Desc;
    use metrics::{Collector, Opts};
    use proto;
    use std::collections::HashMap;
    use std::thread;

    #[test]
    fn test_registry() {
        let r = Registry::new();

        let counter = Counter::new("test", "test help").unwrap();
        r.register(Box::new(counter.clone())).unwrap();
        counter.inc();

        let r1 = r.clone();
        let handler = thread::spawn(move || {
            let metric_familys = r1.gather();
            assert_eq!(metric_familys.len(), 1);
        });

        assert!(handler.join().is_ok());

        assert!(r.register(Box::new(counter.clone())).is_err());
        assert!(r.unregister(Box::new(counter.clone())).is_ok());
        assert!(r.unregister(Box::new(counter.clone())).is_err());

        let counter_vec =
            CounterVec::new(Opts::new("test_vec", "test vec help"), &["a", "b"]).unwrap();

        r.register(Box::new(counter_vec.clone())).unwrap();
        counter_vec.with_label_values(&["1", "2"]).inc();
    }

    #[test]
    fn test_default_registry() {
        let counter = Counter::new("test", "test help").unwrap();

        assert!(register(Box::new(counter.clone())).is_ok());
        assert_ne!(gather().len(), 0);

        assert!(unregister(Box::new(counter.clone())).is_ok());
        assert!(unregister(Box::new(counter.clone())).is_err());
    }

    #[test]
    fn test_gather_order() {
        let r = Registry::new();

        let counter_a = Counter::new("test_a_counter", "test help").unwrap();
        let counter_b = Counter::new("test_b_counter", "test help").unwrap();
        let counter_2 = Counter::new("test_2_counter", "test help").unwrap();
        r.register(Box::new(counter_b.clone())).unwrap();
        r.register(Box::new(counter_2.clone())).unwrap();
        r.register(Box::new(counter_a.clone())).unwrap();

        let mfs = r.gather();
        assert_eq!(mfs.len(), 3);
        assert_eq!(mfs[0].get_name(), "test_2_counter");
        assert_eq!(mfs[1].get_name(), "test_a_counter");
        assert_eq!(mfs[2].get_name(), "test_b_counter");

        let r = Registry::new();
        let opts = Opts::new("test", "test help")
            .const_label("a", "1")
            .const_label("b", "2");
        let counter_vec = CounterVec::new(opts, &["cc", "c1", "a2", "c0"]).unwrap();
        r.register(Box::new(counter_vec.clone())).unwrap();

        let mut map1 = HashMap::new();
        map1.insert("cc", "12");
        map1.insert("c1", "a1");
        map1.insert("a2", "0");
        map1.insert("c0", "hello");
        counter_vec.with(&map1).inc();

        let mut map2 = HashMap::new();
        map2.insert("cc", "12");
        map2.insert("c1", "0");
        map2.insert("a2", "0");
        map2.insert("c0", "hello");
        counter_vec.with(&map2).inc();
        counter_vec.with(&map2).inc();

        let mut map3 = HashMap::new();
        map3.insert("cc", "12");
        map3.insert("c1", "0");
        map3.insert("a2", "da");
        map3.insert("c0", "hello");
        counter_vec.with(&map3).inc();
        counter_vec.with(&map3).inc();
        counter_vec.with(&map3).inc();

        let mut map4 = HashMap::new();
        map4.insert("cc", "12");
        map4.insert("c1", "0");
        map4.insert("a2", "da");
        map4.insert("c0", "你好");
        counter_vec.with(&map4).inc();
        counter_vec.with(&map4).inc();
        counter_vec.with(&map4).inc();
        counter_vec.with(&map4).inc();

        // # HELP test test help
        // # TYPE test counter
        // test{a="1",a2="0",b="2",c0="hello",c1="0",cc="12"} 2
        // test{a="1",a2="0",b="2",c0="hello",c1="a1",cc="12"} 1
        // test{a="1",a2="da",b="2",c0="hello",c1="0",cc="12"} 3
        // test{a="1",a2="da",b="2",c0="你好",c1="0",cc="12"} 4

        let mfs = r.gather();
        assert_eq!(mfs.len(), 1);
        let ms = mfs[0].get_metric();
        assert_eq!(ms.len(), 4);
        assert_eq!(ms[0].get_counter().get_value() as u64, 2);
        assert_eq!(ms[1].get_counter().get_value() as u64, 1);
        assert_eq!(ms[2].get_counter().get_value() as u64, 3);
        assert_eq!(ms[3].get_counter().get_value() as u64, 4);
    }

    struct MultipleCollector {
        descs: Vec<Desc>,
        counters: Vec<Counter>,
    }

    impl Collector for MultipleCollector {
        fn desc(&self) -> Vec<&Desc> {
            self.descs.iter().collect()
        }

        fn collect(&self) -> Vec<proto::MetricFamily> {
            self.counters
                .iter()
                .inspect(|c| c.inc())
                .map(|c| c.collect())
                .fold(Vec::new(), |mut acc, mfs| {
                    acc.extend(mfs);
                    acc
                })
        }
    }

    #[test]
    fn test_register_multiplecollector() {
        let counters = vec![
            Counter::new("c1", "c1 is a counter").unwrap(),
            Counter::new("c2", "c2 is a counter").unwrap(),
        ];

        let descs = counters
            .iter()
            .map(|c| c.desc().into_iter().cloned())
            .fold(Vec::new(), |mut acc, ds| {
                acc.extend(ds);
                acc
            });

        let mc = MultipleCollector {
            descs: descs,
            counters: counters,
        };

        let r = Registry::new();
        r.register(Box::new(mc)).unwrap();
    }
}
