use super::*;

impl Store {
    pub fn tsadd(
        &self,
        key: &[u8],
        timestamp: i64,
        value: f64,
        retention: Option<u64>,
        labels: Option<Vec<(String, String)>>,
        now: Instant,
    ) -> Result<i64, String> {
        let idx = self.shard_index(key);
        let mut shard = self.shards[idx].write();
        shard.version += 1;
        let ks = key_bytes(key);
        let existed = shard.data.contains_key(&ks);
        let entry = shard.data.entry(ks).or_insert_with(|| Entry {
            value: StoreValue::TimeSeries(TimeSeriesData {
                samples: Vec::new(),
                retention: retention.unwrap_or(0),
                labels: labels.clone().unwrap_or_default(),
            }),
            expires_at: None,
            lru_clock: self.lru_clock(),
        });
        if !existed {
            self.key_added();
        }
        if entry.is_expired_at(now) {
            entry.value = StoreValue::TimeSeries(TimeSeriesData {
                samples: Vec::new(),
                retention: retention.unwrap_or(0),
                labels: labels.clone().unwrap_or_default(),
            });
            entry.expires_at = None;
        }
        match &mut entry.value {
            StoreValue::TimeSeries(ts) => {
                if let Some(r) = retention {
                    ts.retention = r;
                }
                if let Some(l) = labels {
                    ts.labels = l;
                }
                let pos = ts.samples.binary_search_by_key(&timestamp, |s| s.0);
                let mut added = 0usize;
                match pos {
                    Ok(i) => ts.samples[i].1 = value,
                    Err(i) => {
                        ts.samples.insert(i, (timestamp, value));
                        added = 16;
                    }
                }
                let mut trimmed = 0usize;
                if ts.retention > 0 {
                    let cutoff = timestamp - ts.retention as i64;
                    let keep_from = ts.samples.partition_point(|s| s.0 < cutoff);
                    if keep_from > 0 {
                        trimmed = keep_from * 16;
                        ts.samples.drain(..keep_from);
                    }
                }
                let _ = entry;
                if added > trimmed {
                    shard.used_memory += added - trimmed;
                    self.mem_add(added - trimmed);
                } else if trimmed > added {
                    let freed = trimmed - added;
                    shard.used_memory = shard.used_memory.saturating_sub(freed);
                    self.mem_sub(freed);
                }
                Ok(timestamp)
            }
            _ => Err(WRONGTYPE.to_string()),
        }
    }

    pub fn tsget(&self, key: &[u8], now: Instant) -> Result<Option<(i64, f64)>, String> {
        let idx = self.shard_index(key);
        let shard = self.shards[idx].read();
        match shard.data.get(key) {
            Some(entry) if !entry.is_expired_at(now) => match &entry.value {
                StoreValue::TimeSeries(ts) => Ok(ts.samples.last().copied()),
                _ => Err(WRONGTYPE.to_string()),
            },
            _ => Ok(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tsrange(
        &self,
        key: &[u8],
        from: i64,
        to: i64,
        agg: Option<(&str, i64)>,
        count: Option<usize>,
        now: Instant,
    ) -> Result<Vec<(i64, f64)>, String> {
        let idx = self.shard_index(key);
        let shard = self.shards[idx].read();
        match shard.data.get(key) {
            Some(entry) if !entry.is_expired_at(now) => match &entry.value {
                StoreValue::TimeSeries(ts) => {
                    let start = ts.samples.partition_point(|s| s.0 < from);
                    let end = ts.samples.partition_point(|s| s.0 <= to);
                    let slice = &ts.samples[start..end];

                    if let Some((agg_fn, bucket_ms)) = agg {
                        let result = aggregate_samples(slice, agg_fn, bucket_ms);
                        Ok(match count {
                            Some(n) => result.into_iter().take(n).collect(),
                            None => result,
                        })
                    } else {
                        Ok(match count {
                            Some(n) => slice.iter().take(n).copied().collect(),
                            None => slice.to_vec(),
                        })
                    }
                }
                _ => Err(WRONGTYPE.to_string()),
            },
            _ => Ok(Vec::new()),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn tsinfo(
        &self,
        key: &[u8],
        now: Instant,
    ) -> Result<
        Option<(
            usize,
            Option<(i64, f64)>,
            Option<(i64, f64)>,
            u64,
            Vec<(String, String)>,
        )>,
        String,
    > {
        let idx = self.shard_index(key);
        let shard = self.shards[idx].read();
        match shard.data.get(key) {
            Some(entry) if !entry.is_expired_at(now) => match &entry.value {
                StoreValue::TimeSeries(ts) => Ok(Some((
                    ts.samples.len(),
                    ts.samples.first().copied(),
                    ts.samples.last().copied(),
                    ts.retention,
                    ts.labels.clone(),
                ))),
                _ => Err(WRONGTYPE.to_string()),
            },
            _ => Ok(None),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn tsmrange(
        &self,
        from: i64,
        to: i64,
        filters: &[(String, String)],
        agg: Option<(&str, i64)>,
        now: Instant,
    ) -> Vec<(String, Vec<(String, String)>, Vec<(i64, f64)>)> {
        let mut results = Vec::new();
        for shard in self.shards.iter() {
            let shard = shard.read();
            for (key, entry) in shard.data.iter() {
                if entry.is_expired_at(now) {
                    continue;
                }
                if let StoreValue::TimeSeries(ts) = &entry.value {
                    let matches = filters
                        .iter()
                        .all(|(fk, fv)| ts.labels.iter().any(|(lk, lv)| lk == fk && lv == fv));
                    if !matches {
                        continue;
                    }
                    let start = ts.samples.partition_point(|s| s.0 < from);
                    let end = ts.samples.partition_point(|s| s.0 <= to);
                    let slice = &ts.samples[start..end];
                    let samples = if let Some((agg_fn, bucket_ms)) = agg {
                        aggregate_samples(slice, agg_fn, bucket_ms)
                    } else {
                        slice.to_vec()
                    };
                    results.push((key_string(key), ts.labels.clone(), samples));
                }
            }
        }
        results
    }
}

#[inline]
fn aggregate_samples(samples: &[(i64, f64)], agg_fn: &str, bucket_ms: i64) -> Vec<(i64, f64)> {
    if samples.is_empty() || bucket_ms <= 0 {
        return Vec::new();
    }
    let first_ts = samples[0].0;
    let mut results = Vec::new();
    let mut bucket_start = (first_ts / bucket_ms) * bucket_ms;
    let mut bucket_vals: Vec<f64> = Vec::new();

    for &(ts, val) in samples {
        while ts >= bucket_start + bucket_ms {
            if !bucket_vals.is_empty() {
                results.push((bucket_start, compute_agg(&bucket_vals, agg_fn)));
                bucket_vals.clear();
            }
            bucket_start += bucket_ms;
        }
        bucket_vals.push(val);
    }
    if !bucket_vals.is_empty() {
        results.push((bucket_start, compute_agg(&bucket_vals, agg_fn)));
    }
    results
}

fn compute_agg(vals: &[f64], agg_fn: &str) -> f64 {
    match agg_fn {
        "avg" => vals.iter().sum::<f64>() / vals.len() as f64,
        "sum" => vals.iter().sum(),
        "min" => vals.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "count" => vals.len() as f64,
        "first" => vals[0],
        "last" => vals[vals.len() - 1],
        "range" => {
            let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            max - min
        }
        "std.p" => {
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
            var.sqrt()
        }
        "std.s" => {
            if vals.len() < 2 {
                return 0.0;
            }
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            let var =
                vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
            var.sqrt()
        }
        "var.p" => {
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64
        }
        "var.s" => {
            if vals.len() < 2 {
                return 0.0;
            }
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vals.len() - 1) as f64
        }
        _ => vals.iter().sum::<f64>() / vals.len() as f64,
    }
}
