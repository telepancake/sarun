use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderBindingContext {
    Document,
    History,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderAction {
    SearchCancel,
    SearchAccept,
    SearchBackspace,
    SearchText,
    ScrollDown,
    ScrollUp,
    FocusHistory,
    NextView,
    RenderedView,
    RawView,
    DiffView,
    SpatialLeft,
    SpatialRight,
    SpatialUp,
    SpatialDown,
    PageDown,
    PageUp,
    Home,
    End,
    FocusNextLink,
    FocusPreviousLink,
    JumpNext,
    JumpPrevious,
    Follow,
    Back,
    Forward,
    DocumentSearch,
    TitleSearch,
    FullTextSearch,
    ToggleFull,
    Open,
    Close,
    HistoryUp,
    HistoryDown,
    HistoryPageUp,
    HistoryPageDown,
    HistoryHome,
    HistoryEnd,
    HistoryOpen,
    HistoryUser,
    HistoryEdits,
    HistoryReturn,
}

/// Chupa's portable reader keymap. Embedders may expose the same actions in
/// additional menus, but document behavior and the standalone GUI share this
/// one mapping.
pub fn reader_action(
    context: ReaderBindingContext,
    _wiki_available: bool,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<ReaderAction> {
    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    use ReaderAction as A;
    use ReaderBindingContext as C;
    match (context, code) {
        (C::Search, KeyCode::Esc) => Some(A::SearchCancel),
        (C::Search, KeyCode::Enter) => Some(A::SearchAccept),
        (C::Search, KeyCode::Backspace) => Some(A::SearchBackspace),
        (C::Search, KeyCode::Char(character)) if !character.is_control() => Some(A::SearchText),

        (C::History, KeyCode::Up | KeyCode::Char('k')) => Some(A::HistoryUp),
        (C::History, KeyCode::Down | KeyCode::Char('j')) => Some(A::HistoryDown),
        (C::History, KeyCode::PageUp) => Some(A::HistoryPageUp),
        (C::History, KeyCode::PageDown) => Some(A::HistoryPageDown),
        (C::History, KeyCode::Home | KeyCode::Char('g')) => Some(A::HistoryHome),
        (C::History, KeyCode::End | KeyCode::Char('G')) => Some(A::HistoryEnd),
        (C::History, KeyCode::Enter) => Some(A::HistoryOpen),
        (C::History, KeyCode::Char('u')) => Some(A::HistoryUser),
        (C::History, KeyCode::Char('e')) => Some(A::HistoryEdits),
        (C::History, KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab | KeyCode::Esc) => {
            Some(A::HistoryReturn)
        }
        (C::History, KeyCode::Char('v')) => Some(A::NextView),
        (C::History, KeyCode::Char('1')) => Some(A::RenderedView),
        (C::History, KeyCode::Char('2')) => Some(A::RawView),
        (C::History, KeyCode::Char('3')) => Some(A::DiffView),
        (C::History, KeyCode::Char('z')) => Some(A::ToggleFull),
        (C::History, KeyCode::Backspace | KeyCode::Char('[')) => Some(A::Back),
        (C::History, KeyCode::Char(']')) => Some(A::Forward),

        (C::Document, KeyCode::Char('j')) => Some(A::ScrollDown),
        (C::Document, KeyCode::Char('k')) => Some(A::ScrollUp),
        (C::Document, KeyCode::Char('h')) => Some(A::FocusHistory),
        (C::Document, KeyCode::Char('v')) => Some(A::NextView),
        (C::Document, KeyCode::Char('1')) => Some(A::RenderedView),
        (C::Document, KeyCode::Char('2')) => Some(A::RawView),
        (C::Document, KeyCode::Char('3')) => Some(A::DiffView),
        (C::Document, KeyCode::Left) => Some(A::SpatialLeft),
        (C::Document, KeyCode::Right) => Some(A::SpatialRight),
        (C::Document, KeyCode::Up) => Some(A::SpatialUp),
        (C::Document, KeyCode::Down) => Some(A::SpatialDown),
        (C::Document, KeyCode::PageDown) => Some(A::PageDown),
        (C::Document, KeyCode::PageUp) => Some(A::PageUp),
        (C::Document, KeyCode::Home | KeyCode::Char('g')) => Some(A::Home),
        (C::Document, KeyCode::End | KeyCode::Char('G')) => Some(A::End),
        (C::Document, KeyCode::Tab) => Some(A::FocusNextLink),
        (C::Document, KeyCode::BackTab) => Some(A::FocusPreviousLink),
        (C::Document, KeyCode::Enter) => Some(A::Follow),
        (C::Document, KeyCode::Backspace | KeyCode::Char('[')) => Some(A::Back),
        (C::Document, KeyCode::Char(']')) => Some(A::Forward),
        (C::Document, KeyCode::Char('n')) => Some(A::JumpNext),
        (C::Document, KeyCode::Char('p')) => Some(A::JumpPrevious),
        (C::Document, KeyCode::Char('/')) => Some(A::DocumentSearch),
        (C::Document, KeyCode::Char('T')) => Some(A::TitleSearch),
        (C::Document, KeyCode::Char('F')) => Some(A::FullTextSearch),
        (C::Document, KeyCode::Char('z')) => Some(A::ToggleFull),
        (C::Document, KeyCode::Char('o')) => Some(A::Open),
        (C::Document, KeyCode::Esc) => Some(A::Close),
        _ => None,
    }
}

pub fn reader_context_hint(context: ReaderBindingContext, wiki_available: bool) -> String {
    match context {
        ReaderBindingContext::Document if wiki_available => {
            "↑↓ links · Enter follow · h history · T titles · F full text · / search · z fullscreen"
        }
        ReaderBindingContext::Document => {
            "↑↓ links · Enter follow · Tab next link · / search · o open · z fullscreen"
        }
        ReaderBindingContext::History => {
            "↑↓ revisions · Enter open · u user · e edits · v view · → document"
        }
        ReaderBindingContext::Search => "type query · Enter accept · Esc cancel",
    }
    .to_string()
}
