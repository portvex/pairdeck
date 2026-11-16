//! Progress dialog for a running file operation.

use super::{Dialog, Outcome, centered, draw_box};
use crate::ops::JobProgress;
use crate::ui::Ctx;
use crate::util::{human_size, trunc_left};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Gauge, Paragraph},
};
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

pub struct ProgressDialog {
    job: Arc<JobProgress>,
    focus: usize,
}

impl ProgressDialog {
    pub fn new(job: Arc<JobProgress>) -> Self {
        ProgressDialog { job, focus: 0 }
    }
}

fn fmt_dur(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl Dialog for ProgressDialog {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let j = &self.job;
        let r = centered(area, 70, 13);
        let inner = draw_box(f, r, &j.title, ctx, false);
        let w = inner.width as usize;
        let cur = j.current.lock().unwrap().clone();
        let tgt = j.target.lock().unwrap().clone();
        let mut lines = vec![
            Line::styled(format!("Source: {}", trunc_left(&cur, w.saturating_sub(8))), ctx.theme.dialog),
            Line::styled(
                if tgt.is_empty() {
                    String::new()
                } else {
                    format!("Target: {}", trunc_left(&tgt, w.saturating_sub(8)))
                },
                ctx.theme.dialog,
            ),
        ];
        if j.scanning.load(Relaxed) {
            lines.push(Line::styled("Scanning...", ctx.theme.dialog_title));
        }
        f.render_widget(Paragraph::new(lines), Rect { height: 3, ..inner });
        let fs = j.file_size.load(Relaxed);
        let fd = j.file_done.load(Relaxed);
        let file_ratio = if fs == 0 { 0.0 } else { (fd as f64 / fs as f64).min(1.0) };
        f.render_widget(
            Gauge::default()
                .gauge_style(ctx.theme.gauge)
                .ratio(file_ratio)
                .label(format!("{:.0}%", file_ratio * 100.0)),
            Rect { y: inner.y + 3, height: 1, ..inner },
        );
        let total = j.fraction().min(1.0);
        f.render_widget(
            Gauge::default().gauge_style(ctx.theme.gauge).ratio(total).label(format!("Total {:.0}%", total * 100.0)),
            Rect { y: inner.y + 5, height: 1, ..inner },
        );
        let eta = j.eta().map(fmt_dur).unwrap_or_else(|| "--:--".into());
        let stats = format!(
            "Files {}/{}   {} / {}   {}/s   elapsed {}   ETA {}",
            j.done_files.load(Relaxed),
            j.total_files.load(Relaxed),
            human_size(j.done_bytes.load(Relaxed)),
            human_size(j.total_bytes.load(Relaxed)),
            human_size(j.bytes_per_sec() as u64),
            fmt_dur(j.started.elapsed()),
            eta
        );
        f.render_widget(
            Paragraph::new(Span::styled(stats, ctx.theme.dialog)),
            Rect { y: inner.y + 7, height: 1, ..inner },
        );
        let paused = j.paused.load(Relaxed);
        let labels = ["Background", if paused { "Resume" } else { "Pause" }, "Abort"];
        let mut x = inner.x + (inner.width.saturating_sub(40)) / 2;
        let y = inner.y + inner.height.saturating_sub(1);
        for (i, l) in labels.iter().enumerate() {
            let t = format!("[ {l} ]");
            let st = if i == self.focus { ctx.theme.dialog_focus } else { ctx.theme.dialog };
            let tw = t.len() as u16;
            f.render_widget(Paragraph::new(Span::styled(t, st)), Rect { x, y, width: tw, height: 1 });
            x += tw + 1;
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('b') => return Outcome::Close,
            KeyCode::Left | KeyCode::BackTab => self.focus = (self.focus + 2) % 3,
            KeyCode::Right | KeyCode::Tab => self.focus = (self.focus + 1) % 3,
            KeyCode::Char('p') | KeyCode::Char('r') => {
                let p = self.job.paused.load(Relaxed);
                self.job.paused.store(!p, Relaxed);
            }
            KeyCode::Char('a') => {
                self.job.cancel.store(true, Relaxed);
                return Outcome::Close;
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.focus {
                0 => return Outcome::Close,
                1 => {
                    let p = self.job.paused.load(Relaxed);
                    self.job.paused.store(!p, Relaxed);
                }
                _ => {
                    self.job.cancel.store(true, Relaxed);
                    return Outcome::Close;
                }
            },
            _ => {}
        }
        Outcome::Keep
    }

    fn tick(&mut self) -> Outcome {
        if self.job.finished.load(Relaxed) { Outcome::Close } else { Outcome::Keep }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
