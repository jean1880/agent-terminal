//! Called by the existing isolated window walkthrough, on GTK's owning thread.
use super::*;
use crate::testutil::pump_until;

pub(in crate::window::imp::threads) fn cancelled_start_can_retry(
    window: &crate::window::AgentTerminalWindow,
) {
    let imp = window.imp();
    let availability = AgentAvailability::shared();
    let availability_before = availability.all();
    for driver in Driver::ALL {
        availability.set(driver, Availability::Missing);
    }
    let page = imp
        .create_chat_thread(NewThread::new(Driver::Claude))
        .unwrap();
    let thread = imp
        .tabs
        .borrow()
        .iter()
        .find(|tab| tab.page == page)
        .unwrap()
        .chat
        .as_ref()
        .unwrap()
        .thread
        .clone();
    let slot = imp.slot_of(&thread).unwrap();
    let ctx = glib::MainContext::default();
    assert!(pump_until(&ctx, 5, || imp
        .tabs
        .borrow()
        .iter()
        .filter_map(|t| t.chat.as_ref())
        .any(|c| c.thread == thread && c.view.is_some() && !c.building)));
    let starting = || {
        imp.tabs
            .borrow()
            .iter()
            .filter_map(|tab| tab.chat.as_ref())
            .find(|chat| chat.thread == thread)
            .unwrap()
            .starting
    };
    let cancelled = imp.prepare_session_start(&thread, &slot);
    assert!(
        starting(),
        "the first start is reserved before preparation yields"
    );
    assert!(cancelled.current(imp, &thread));
    let provider_before = app_store().active_provider_thread(&thread).unwrap();
    imp.switch_in_place(&thread, Driver::Codex);
    assert!(
        cancelled.current(imp, &thread),
        "a parked switch does not replace the pending launch"
    );
    assert_eq!(
        app_store().active_provider_thread(&thread).unwrap(),
        provider_before
    );
    {
        let mut tabs = imp.tabs.borrow_mut();
        let chat = tabs
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
            .unwrap();
        assert!(
            !chat.rebinding,
            "switching during the first start must not write another provider"
        );
        assert_eq!(
            chat.pending_switch.take(),
            Some((Driver::Codex, None, None))
        );
    }
    slot.interrupt(); // The real Stop path advances the same serial as mode/switch/transfer.
    assert!(!cancelled.current(imp, &thread));
    cancelled.clear_cancelled(imp, &thread);
    assert!(
        !starting(),
        "Stop during preparation must allow the next start"
    );
    let retry = imp.prepare_session_start(&thread, &slot);
    assert!(
        retry.current(imp, &thread),
        "a cancelled serial must not poison retries"
    );
    cancelled.clear_cancelled(imp, &thread);
    assert!(
        starting(),
        "a late old completion cannot clear a newer start"
    );
    let newer = imp.prepare_session_start(&thread, &slot);
    assert!(
        !retry.current(imp, &thread),
        "same-serial preparation was superseded"
    );
    assert!(newer.current(imp, &thread));
    retry.clear_cancelled(imp, &thread);
    assert!(starting());
    slot.invalidate_pending_switch();
    assert!(!newer.current(imp, &thread));
    newer.clear_cancelled(imp, &thread);
    assert!(!starting());
    imp.close_row_now(&RowKey::Thread(thread));
    for (driver, state) in availability_before {
        availability.set(driver, state);
    }
}

pub(in crate::window::imp::threads) fn transfer_keeps_live_view_routes(
    source: &crate::window::AgentTerminalWindow,
    thread: &str,
    slot: &Rc<SessionSlot>,
) {
    let ctx = glib::MainContext::default();
    let app = source
        .application()
        .unwrap()
        .downcast::<adw::Application>()
        .unwrap();
    let destination = crate::window::AgentTerminalWindow::new(&app);
    assert!(pump_until(&ctx, 5, || destination
        .imp()
        .tab_view
        .borrow()
        .is_some()));
    destination.imp().finish_shell(None);
    destination.imp().reveal_shell();
    destination.present();
    // A window no open thread is registered to is still searched for tabs in a workspace.
    assert!(
        super::every_window().contains(&destination),
        "a window without threads is checked before a workspace is removed"
    );
    let session = slot.get().unwrap();
    let page = source.imp().page_of_thread(thread).unwrap();
    let chat_view = source
        .imp()
        .tabs
        .borrow()
        .iter()
        .filter_map(|tab| tab.chat.as_ref())
        .find(|chat| chat.thread == thread)
        .unwrap()
        .view
        .clone()
        .unwrap();
    let source_tabs = source.imp().tab_view.borrow().clone().unwrap();
    let destination_tabs = destination.imp().tab_view.borrow().clone().unwrap();
    let old_generation = session.process_generation();
    let old_watch = session.exit_watch().unwrap();
    let old_serial = slot.switch_serial.get();
    source_tabs.transfer_page(&page, &destination_tabs, 0);
    assert!(source.imp().page_of_thread(thread).is_none());
    assert_eq!(thread_owner(thread).as_ref(), Some(&destination));
    assert!(Rc::ptr_eq(
        &destination.imp().slot_of(thread).unwrap(),
        slot
    ));
    assert!(
        slot.switch_serial.get() != old_serial,
        "transfer invalidates source preparation"
    );
    assert_eq!(window_of(&chat_view).as_ref(), Some(&destination));
    assert!(
        pump_until(&ctx, 15, || old_watch.phase()
            == crate::agent_proc::StopPhase::Exited
            && session.process_generation() > old_generation
            && session.status().alive),
        "transfer must stop its old process and resume the same session in a new generation"
    );

    // The original view's action handler must resolve its owner when invoked, rather than
    // retaining the source window captured at construction. Submit through its real composer.
    let children = crate::testutil::descendants(chat_view.upcast_ref());
    let field = children
        .iter()
        .find_map(|widget| widget.downcast_ref::<gtk4::TextView>())
        .unwrap();
    let send = children
        .iter()
        .find(|widget| widget.has_css_class("send-button"))
        .unwrap()
        .clone()
        .downcast::<gtk4::Button>()
        .unwrap();
    let source_count = source.imp().thread_pages();
    let destination_count = destination.imp().thread_pages();
    field.buffer().set_text("/new");
    send.emit_clicked();
    assert!(pump_until(&ctx, 5, || destination.imp().thread_pages()
        == destination_count + 1));
    assert_eq!(
        source.imp().thread_pages(),
        source_count,
        "view action reached the source window"
    );
    let extra = destination
        .imp()
        .tabs
        .borrow()
        .iter()
        .filter_map(|tab| tab.chat.as_ref())
        .find(|chat| chat.thread != thread)
        .unwrap()
        .thread
        .clone();
    destination.imp().close_row_now(&RowKey::Thread(extra));
    destination_tabs.set_selected_page(&page);

    // An approval keeps the fixture turn open. Root envelopes must update the destination's
    // busy state through the already-created session sink, while the source has no such tab.
    slot.send_prompt("approve after transfer");
    assert!(pump_until(&ctx, 15, || destination
        .imp()
        .tabs
        .borrow()
        .iter()
        .filter_map(|tab| tab.chat.as_ref())
        .any(|chat| chat.thread == thread
            && chat.running
            && chat.approval)));
    slot.interrupt();
    assert!(pump_until(&ctx, 15, || destination
        .imp()
        .tabs
        .borrow()
        .iter()
        .filter_map(|tab| tab.chat.as_ref())
        .any(|chat| chat.thread == thread
            && !chat.running
            && !chat.approval)));
    let moved_generation = session.process_generation();
    destination_tabs.transfer_page(&page, &source_tabs, 0);
    assert_eq!(thread_owner(thread).as_ref(), Some(source));
    assert!(Rc::ptr_eq(&source.imp().slot_of(thread).unwrap(), slot));
    assert!(pump_until(&ctx, 15, || session.process_generation()
        > moved_generation
        && session.status().alive));
    source_tabs.set_selected_page(&page);
    destination.destroy();
}
