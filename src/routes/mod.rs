pub mod chain;
pub mod duplicate;
pub mod duplicate_hls;
pub mod media;
pub mod mirror;
pub mod move2nsfw;
pub mod upload;
pub mod videogen;

#[cfg(test)]
mod tests {
    use super::videogen;

    #[test]
    fn videogen_drafts_handler_is_exported() {
        let _ = videogen::get_in_progress_drafts;
    }
}
