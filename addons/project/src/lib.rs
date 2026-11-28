// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `project`: projects, their tasks, per-project task stages and tags.
//!
//! Every project has its own stages: a new project copies the template stages (stages without a
//! project), and a task only uses stages of its own project, which lets the task board show the
//! selected project's columns.
erp_sdk::addon!(name = "project", depends = ["base", "web", "mail"]);

mod counts;
pub mod project;
pub mod stage;
pub mod tags;
pub mod task;

pub use project::ProjectProject;
pub use stage::ProjectTaskType;
pub use tags::ProjectTags;
pub use task::ProjectTask;

erp_sdk::model_ref!(pub MailThread = "mail.thread");
erp_sdk::model_ref!(pub MailActivityMixin = "mail.activity.mixin");

/// The task states that end the work: a stage move keeps them, and open counts leave them out.
pub const CLOSED_STATES: &[&str] = &["done", "cancelled"];
