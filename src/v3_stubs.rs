//! TEMPORARY stand-ins for the model catalogue and account status being built on `v3/catalog`.
//!
//! The window codes against the same names (`ModelCatalog::shared().refresh(..)`,
//! `ChatView::set_model_source`, `AccountStatus::shared().observe(..)`,
//! `ChatView::set_account_status`, `UsageIndicator`). When `v3/catalog` is merged, delete this
//! file and its `mod` line, and point the window's `use crate::v3_stubs::…` at
//! `crate::model_catalog`, `crate::account_status`, `crate::chat::view::usage` and
//! `agent_core::catalog`. The duplicate `ChatView` methods then fail to compile until it is.

use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::event::Envelope;
use gtk4::prelude::*;

use crate::chat::view::ChatView;

/// Mirror of `agent_core::catalog::CatalogModel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub driver: Driver,
    pub id: String,
    pub display: String,
    pub description: Option<String>,
    pub efforts: Vec<String>,
    pub via: Option<String>,
}

/// Stand-in for `crate::model_catalog::ModelCatalog`: always empty.
#[derive(Default)]
pub struct ModelCatalog;

thread_local! {
    static CATALOG: Rc<ModelCatalog> = Rc::default();
    static ACCOUNT: Rc<AccountStatus> = Rc::default();
}

impl ModelCatalog {
    pub fn shared() -> Rc<ModelCatalog> {
        CATALOG.with(Rc::clone)
    }
    pub fn models(&self) -> Vec<CatalogModel> {
        Vec::new()
    }
    pub fn refresh(self: &Rc<Self>, _claude_program: &str, _agy_program: &str) {}
    pub fn connect_changed(&self, _f: impl Fn() + 'static) {}
}

/// Stand-in for `crate::account_status::AccountStatus`.
#[derive(Default)]
pub struct AccountStatus;

impl AccountStatus {
    pub fn shared() -> Rc<AccountStatus> {
        ACCOUNT.with(Rc::clone)
    }
    pub fn observe(&self, _driver: Driver, _env: &Envelope) {}
    pub fn refresh(self: &Rc<Self>, _claude_program: &str, _agy_program: &str) {}
    pub fn connect_changed(&self, _f: impl Fn() + 'static) {}
}

/// Stand-in for `crate::chat::view::usage::UsageIndicator`: an empty box.
pub struct UsageIndicator {
    root: gtk4::Box,
}

impl UsageIndicator {
    pub fn new(_status: Rc<AccountStatus>, _driver: Option<Driver>) -> Self {
        Self {
            root: gtk4::Box::new(gtk4::Orientation::Horizontal, 0),
        }
    }
    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }
}

impl ChatView {
    pub fn set_model_source(&self, _source: Rc<ModelCatalog>) {}
    pub fn set_account_status(&self, _status: Rc<AccountStatus>) {}
}
