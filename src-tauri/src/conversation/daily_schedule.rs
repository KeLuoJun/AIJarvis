use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    course_notes::{CourseSummaryError, CourseSummaryModel},
    error::{AppError, ErrorCode},
};

const MAX_HOUR_IMAGES: usize = 3;
const MAX_HOUR_RAW_IMAGES: usize = 120;
const MAX_DAILY_IMAGES: usize = 12;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySummaryImage {
    pub name: String,
    pub data_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySummary {
    pub date: String,
    pub title: String,
    pub markdown: String,
    pub images: Vec<DailySummaryImage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySummaryListItem {
    pub date: String,
    pub title: String,
    pub image_count: usize,
}

#[derive(Default)]
struct DailyScheduleState {
    last_day: Option<i64>,
    last_hour: Option<u8>,
    last_hash: Option<[u8; 32]>,
    processing: BTreeSet<String>,
}

pub struct DailySchedule {
    root: PathBuf,
    model: Arc<dyn CourseSummaryModel>,
    state: Mutex<DailyScheduleState>,
}

impl DailySchedule {
    pub fn new(root: PathBuf, model: Arc<dyn CourseSummaryModel>) -> Arc<Self> {
        let _ = fs::create_dir_all(root.join("cache"));
        let _ = fs::create_dir_all(root.join("summaries"));
        Arc::new(Self {
            root,
            model,
            state: Mutex::new(DailyScheduleState::default()),
        })
    }

    pub fn record(self: &Arc<Self>, bytes: &[u8], now: SystemTime) {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return;
        }
        let (day, hour) = day_hour(now);
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let previous_day = {
            let mut state = self.lock_state();
            let previous_day = state.last_day;
            if previous_day == Some(day) && state.last_hash == Some(hash) {
                return;
            }
            if previous_day != Some(day) {
                state.last_hash = None;
            }
            if state.last_hour.is_some_and(|value| value != hour) {
                if let Some(old_hour) = state.last_hour {
                    finalize_hour(&self.cache_day(day), old_hour);
                }
            }
            state.last_day = Some(day);
            state.last_hour = Some(hour);
            state.last_hash = Some(hash);
            previous_day.filter(|value| *value != day)
        };

        let directory = self.cache_day(day).join(format!("hour-{hour:02}"));
        if fs::create_dir_all(&directory).is_err() {
            return;
        }
        let millis = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = directory.join(format!("frame-{millis:013}.png"));
        let _ = fs::write(path, bytes);
        prune_hour(&directory);

        if let Some(completed_day) = previous_day {
            let date = date_key(completed_day);
            self.finish_hourly(completed_day);
            let service = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let _ = service.generate(date).await;
            });
        }
    }

    pub async fn generate(&self, date: String) -> Result<DailySummary, AppError> {
        if !valid_date(&date) {
            return Err(AppError::invalid_argument("日程日期格式无效。"));
        }
        {
            let mut state = self.lock_state();
            if !state.processing.insert(date.clone()) {
                return Err(AppError::new(
                    ErrorCode::Internal,
                    "该日期正在生成日程总结。",
                ));
            }
        }
        let result = self.generate_inner(&date).await;
        self.lock_state().processing.remove(&date);
        result
    }

    pub fn list(&self) -> Vec<DailySummaryListItem> {
        let root = self.root.join("summaries");
        let mut items = fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let date = entry.file_name().to_str()?.to_owned();
                if !valid_date(&date) {
                    return None;
                }
                let markdown = fs::read_to_string(entry.path().join("summary.md")).ok()?;
                let title = markdown
                    .lines()
                    .find_map(|line| line.strip_prefix("# "))
                    .unwrap_or("每日日程")
                    .trim()
                    .to_owned();
                let image_count = list_images(&entry.path().join("images")).len();
                Some(DailySummaryListItem {
                    date,
                    title,
                    image_count,
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| b.date.cmp(&a.date));
        items
    }

    pub fn read(&self, date: &str) -> Result<Option<DailySummary>, AppError> {
        if !valid_date(date) {
            return Err(AppError::invalid_argument("日程日期格式无效。"));
        }
        let directory = self.root.join("summaries").join(date);
        let Some(markdown) = fs::read_to_string(directory.join("summary.md")).ok() else {
            return Ok(None);
        };
        let title = markdown
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .unwrap_or("每日日程")
            .trim()
            .to_owned();
        let images = list_images(&directory.join("images"))
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                let bytes = fs::read(path).ok()?;
                Some(DailySummaryImage {
                    name,
                    data_url: format!("data:image/png;base64,{}", STANDARD.encode(bytes)),
                })
            })
            .collect();
        Ok(Some(DailySummary {
            date: date.to_owned(),
            title,
            markdown,
            images,
        }))
    }

    pub fn delete(&self, date: &str) -> Result<(), AppError> {
        if !valid_date(date) {
            return Err(AppError::invalid_argument("日程日期格式无效。"));
        }
        if self.lock_state().processing.contains(date) {
            return Err(AppError::new(
                ErrorCode::Internal,
                "该日期正在生成日程总结。",
            ));
        }
        let directory = self.root.join("summaries").join(date);
        if directory.exists() {
            fs::remove_dir_all(directory)
                .map_err(|_| AppError::new(ErrorCode::ConfigWrite, "日程总结删除失败。"))?;
        }
        Ok(())
    }

    fn finish_hourly(&self, day: i64) {
        let directory = self.cache_day(day);
        for hour in 0..24 {
            finalize_hour(&directory, hour);
        }
    }

    async fn generate_inner(&self, date: &str) -> Result<DailySummary, AppError> {
        let day =
            parse_date(date).ok_or_else(|| AppError::invalid_argument("日程日期格式无效。"))?;
        self.finish_hourly(day);
        let cache = self.cache_day(day);
        let mut groups: BTreeMap<u8, Vec<PathBuf>> = BTreeMap::new();
        for hour in 0..24u8 {
            let images = list_images(&cache.join(format!("hour-{hour:02}")));
            if !images.is_empty() {
                groups.entry(hour / 4).or_default().extend(images);
            }
        }
        if groups.is_empty() {
            return Err(AppError::new(
                ErrorCode::Internal,
                "当天没有可用于总结的截图。",
            ));
        }
        let mut selected = Vec::new();
        let mut sections = Vec::new();
        for (group, paths) in groups {
            // Six four-hour groups fit the daily multimodal budget with two
            // representative frames each while preserving the whole day.
            let paths = evenly_spaced(&paths, 2);
            let mut names = Vec::new();
            let mut data = Vec::new();
            for path in paths {
                let Some(original) = path.file_name().and_then(|v| v.to_str()) else {
                    continue;
                };
                let name = format!("g{group:02}-{original}");
                let bytes = fs::read(&path)
                    .map_err(|_| AppError::new(ErrorCode::Internal, "读取日程截图失败。"))?;
                names.push(name.clone());
                data.push(format!("data:image/png;base64,{}", STANDARD.encode(&bytes)));
                selected.push((name, bytes));
            }
            if !data.is_empty() {
                let prompt = format!(
                    "这是 {date} 的电脑使用日程，第 {start:02}:00-{end:02}:00 时段。请依据截图写出该时段的简洁中文记录，保留时间顺序，不要臆测。",
                    start = group * 4,
                    end = group * 4 + 4
                );
                let summary = self
                    .model
                    .summarize(prompt, names, data)
                    .await
                    .map_err(summary_error)?;
                sections.push(format!(
                    "## {:02}:00—{:02}:00\n\n{}",
                    group * 4,
                    group * 4 + 4,
                    summary.markdown.trim()
                ));
            }
        }
        debug_assert!(selected.len() <= MAX_DAILY_IMAGES);
        let directory = self.root.join("summaries").join(date);
        if directory.exists() {
            fs::remove_dir_all(&directory)
                .map_err(|_| AppError::new(ErrorCode::ConfigWrite, "旧的日程归档清理失败。"))?;
        }
        let images_dir = directory.join("images");
        fs::create_dir_all(&images_dir)
            .map_err(|_| AppError::new(ErrorCode::ConfigWrite, "日程归档目录创建失败。"))?;
        for (name, bytes) in &selected {
            fs::write(images_dir.join(name), bytes)
                .map_err(|_| AppError::new(ErrorCode::ConfigWrite, "日程截图归档失败。"))?;
        }
        let title = format!("{date} 日程总结");
        let markdown = format!("# {title}\n\n{}\n", sections.join("\n\n"));
        fs::write(directory.join("summary.md"), &markdown)
            .map_err(|_| AppError::new(ErrorCode::ConfigWrite, "日程 Markdown 写入失败。"))?;
        let _ = fs::remove_dir_all(cache);
        self.read(date)?
            .ok_or_else(|| AppError::new(ErrorCode::Internal, "日程总结读取失败。"))
    }

    fn cache_day(&self, day: i64) -> PathBuf {
        self.root.join("cache").join(date_key(day))
    }

    fn lock_state(&self) -> MutexGuard<'_, DailyScheduleState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn summary_error(error: CourseSummaryError) -> AppError {
    AppError::new(
        ErrorCode::Internal,
        match error {
            CourseSummaryError::ContextTooLarge => "日程截图过多，模型上下文不足。",
            CourseSummaryError::NotReady => "本地模型尚未就绪。",
            CourseSummaryError::Failed => "日程总结生成失败。",
        },
    )
}

fn day_hour(now: SystemTime) -> (i64, u8) {
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    (
        seconds.div_euclid(86_400),
        (seconds.rem_euclid(86_400) / 3_600) as u8,
    )
}

fn date_key(day: i64) -> String {
    let (year, month, day) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day:02}")
}

fn parse_date(value: &str) -> Option<i64> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && (1..=12).contains(&month) && (1..=31).contains(&day))
        .then(|| days_from_civil(year, month, day))
}

fn valid_date(value: &str) -> bool {
    parse_date(value).is_some() && date_key(parse_date(value).unwrap()) == value
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m, d)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn finalize_hour(day_dir: &Path, hour: u8) {
    let directory = day_dir.join(format!("hour-{hour:02}"));
    let images = list_images(&directory);
    if images.len() <= MAX_HOUR_IMAGES {
        return;
    }
    let keep = evenly_spaced(&images, MAX_HOUR_IMAGES)
        .into_iter()
        .collect::<BTreeSet<_>>();
    for image in images {
        if !keep.contains(&image) {
            let _ = fs::remove_file(image);
        }
    }
}

fn prune_hour(directory: &Path) {
    let images = list_images(directory);
    let excess = images.len().saturating_sub(MAX_HOUR_RAW_IMAGES);
    for image in images.into_iter().take(excess) {
        let _ = fs::remove_file(image);
    }
}

fn list_images(directory: &Path) -> Vec<PathBuf> {
    let mut images = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("png"))
        .collect::<Vec<_>>();
    images.sort();
    images
}

fn evenly_spaced<T: Clone>(values: &[T], limit: usize) -> Vec<T> {
    if values.len() <= limit {
        return values.to_vec();
    }
    if limit == 1 {
        return values.first().cloned().into_iter().collect();
    }
    (0..limit)
        .map(|index| values[index * (values.len() - 1) / (limit - 1)].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_round_trip_is_stable() {
        for value in ["1970-01-01", "2026-09-11", "2000-02-29"] {
            assert_eq!(date_key(parse_date(value).unwrap()), value);
        }
    }

    #[test]
    fn hourly_selection_keeps_at_most_three_frames() {
        assert_eq!(evenly_spaced(&[1, 2, 3, 4, 5], 3), vec![1, 3, 5]);
    }
}
