//! Deciding how much of a half-written reply is worth saying out loud yet.
//!
//! The model writes a sentence over a second or two, and text-to-speech costs a fixed
//! few seconds per request whatever the length. Waiting for the whole reply adds the one
//! to the other; cutting at every word pays the fixed cost per word. What this module
//! finds is the middle: the largest piece that is already complete enough to speak.
//!
//! Everything here is a pure function of the text pushed into it. It reads no clock and
//! calls nothing — which is what lets the hardest judgement in the speech path, where a
//! sentence ends when you have only seen half of it, be tested against fixed strings.
//!
//! Mirrors [`crate::endpoint`] deliberately: same shape, same reason. Push everything,
//! take back whatever is ready, flush what is left at the end.

/// Below this, a piece is not worth its own request.
///
/// Erring small is right — requests overlap, so a short first clause is heard *sooner*,
/// not later. The floor is only about interjections: "Yes." and "Sure." cost the same
/// few-second round trip as a whole sentence, and spending one on four characters buys
/// nothing. Four or five words is where that stops being true.
const MIN_CLAUSE: usize = 16;

/// Above this, speak regardless of punctuation.
///
/// A model that writes a long run without a comma would otherwise be buffered to the end
/// of the reply, which is the exact wait this module exists to avoid. Cutting mid-phrase
/// sounds slightly abrupt; waiting eight seconds in silence sounds broken.
const MAX_CLAUSE: usize = 200;

/// Ends a sentence. Speech pauses here naturally, so a cut is inaudible.
const SENTENCE_END: [char; 3] = ['.', '!', '?'];

/// Ends a clause. A cut here is audible as a slightly long pause, which is the price of
/// starting to speak sooner.
const CLAUSE_END: [char; 3] = [',', ';', ':'];

/// Accumulates a reply as it is written and hands back what is ready to speak.
#[derive(Debug, Default)]
pub struct Clauses {
    pending: String,
}

impl Clauses {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next fragment of the reply, and returns everything now speakable.
    ///
    /// A `Vec` rather than an `Option` because one fragment can complete more than one
    /// clause: the model emits whole sentences at a time often enough that returning
    /// only the first would leave the second waiting for a fragment that may never come.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);

        let mut ready = Vec::new();
        while let Some(cut) = self.next_cut() {
            let rest = self.pending.split_off(cut);
            let clause = std::mem::replace(&mut self.pending, rest);
            ready.push(clause.trim().to_owned());
        }
        ready
    }

    /// Takes whatever is left when the reply ends.
    ///
    /// The last clause of a reply usually has no whitespace after its full stop, so
    /// [`Self::next_cut`] cannot see the boundary and holds it. Without this the final
    /// sentence of every answer would be silently dropped.
    pub fn flush(&mut self) -> Option<String> {
        let remaining = std::mem::take(&mut self.pending);
        let remaining = remaining.trim();
        (!remaining.is_empty()).then(|| remaining.to_owned())
    }

    /// Where the pending text should be cut, if anywhere.
    ///
    /// Sentence breaks are preferred over clause breaks at the same length because they
    /// are the ones a listener does not notice. The length ceiling is checked last, so a
    /// punctuated cut is always taken over an arbitrary one.
    fn next_cut(&self) -> Option<usize> {
        let mut clause_cut = None;

        for (index, character) in self.pending.char_indices() {
            let after = index + character.len_utf8();

            // A cut needs the character *after* the punctuation to have arrived, and to
            // be whitespace. That is what separates "one. Two" from "3.5" — and why the
            // final sentence of a reply is left for `flush`: its full stop is the last
            // character, so the boundary never appears.
            let Some(next) = self.pending[after..].chars().next() else { break };
            if !next.is_whitespace() || after < MIN_CLAUSE {
                continue;
            }

            if SENTENCE_END.contains(&character) {
                return Some(after);
            }
            if CLAUSE_END.contains(&character) {
                clause_cut.get_or_insert(after);
            }
        }

        // A clause break is worth taking only once the text is long enough that the
        // remainder is unlikely to reach a sentence break soon.
        clause_cut
            .filter(|_| self.pending.len() >= MAX_CLAUSE / 2)
            .or_else(|| self.overlong_cut())
    }

    /// The last word boundary before the ceiling, for text with no punctuation in it.
    ///
    /// Cutting at a word boundary rather than at exactly [`MAX_CLAUSE`] bytes keeps the
    /// two halves pronounceable and keeps the index on a character boundary.
    fn overlong_cut(&self) -> Option<usize> {
        (self.pending.len() >= MAX_CLAUSE)
            .then(|| self.pending[..MAX_CLAUSE].rfind(char::is_whitespace))
            .flatten()
            .filter(|cut| *cut >= MIN_CLAUSE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds text one small piece at a time, the way the model actually writes it, and
    /// collects everything that came out.
    fn stream(fragments: &[&str]) -> Vec<String> {
        let mut clauses = Clauses::new();
        let mut spoken: Vec<String> = fragments.iter().flat_map(|f| clauses.push(f)).collect();
        spoken.extend(clauses.flush());
        spoken
    }

    /// The whole point: something is speakable before the reply is finished.
    #[test]
    fn a_finished_sentence_is_spoken_before_the_next_one_is_written() {
        let mut clauses = Clauses::new();
        let ready = clauses.push("I have run the tests for you. They all");

        assert_eq!(ready, vec!["I have run the tests for you."]);
        // The unfinished sentence is still being held, not spoken half-said.
        assert_eq!(clauses.flush().as_deref(), Some("They all"));
    }

    #[test]
    fn nothing_is_lost_and_the_order_is_kept() {
        let spoken = stream(&["I have run the tests. ", "Two of them failed. ", "Want details?"]);
        assert_eq!(
            spoken,
            vec![
                "I have run the tests.",
                "Two of them failed.",
                "Want details?"
            ]
        );
    }

    /// A fragment can complete more than one clause at once, so returning only the
    /// first would leave the second waiting on a fragment that may never come.
    #[test]
    fn one_fragment_can_yield_several_clauses() {
        let mut clauses = Clauses::new();
        let ready = clauses
            .push("The build finished green on the first attempt. Every one of the tests passed too. Now");

        assert_eq!(
            ready,
            vec![
                "The build finished green on the first attempt.",
                "Every one of the tests passed too."
            ]
        );
    }

    /// A full stop inside a number is not the end of a sentence.
    #[test]
    fn a_decimal_point_does_not_end_a_sentence() {
        let spoken = stream(&["The coverage came out at 84.5 percent overall today."]);
        assert_eq!(spoken, vec!["The coverage came out at 84.5 percent overall today."]);
    }

    /// Otherwise every "Yes." and "Sure." costs a whole synthesis round trip.
    #[test]
    fn a_very_short_sentence_waits_for_company() {
        let mut clauses = Clauses::new();
        assert!(clauses.push("Yes. ").is_empty(), "spoke a two-word clause alone");

        // ...but it is never lost: the rest of the reply carries it along.
        let ready = clauses.push("The tests passed on the first attempt. ");
        assert_eq!(ready, vec!["Yes. The tests passed on the first attempt."]);
    }

    /// A reply that ends without trailing whitespace — which is every reply.
    #[test]
    fn the_last_sentence_is_not_dropped() {
        let spoken = stream(&["Everything is working as expected now."]);
        assert_eq!(spoken, vec!["Everything is working as expected now."]);
    }

    #[test]
    fn a_reply_of_only_whitespace_yields_nothing_to_say() {
        assert!(stream(&["   ", "\n"]).is_empty());
    }

    /// The failure this ceiling exists to prevent: a monologue with no full stop in it
    /// buffered all the way to the end of the reply.
    #[test]
    fn an_unpunctuated_monologue_is_still_cut() {
        let long = "so the way this works is that the agent listens and then it thinks \
                    about what you said and after that it produces an answer which then \
                    gets spoken back to you through the very same connection you called on";
        let mut clauses = Clauses::new();
        let ready = clauses.push(long);

        assert!(!ready.is_empty(), "held {} bytes with no cut", long.len());
        assert!(
            ready.iter().all(|clause| clause.len() <= MAX_CLAUSE),
            "a cut piece is still over the ceiling: {ready:?}"
        );
        // Cut between words, so neither half is an unpronounceable fragment.
        assert!(long.starts_with(&ready[0]), "cut mid-word: {:?}", ready[0]);
    }

    /// Cutting at a comma is worth an audible pause only once the sentence is long
    /// enough that waiting for the full stop would cost more than the pause does.
    #[test]
    fn a_comma_cuts_a_long_sentence_but_not_a_short_one() {
        let mut short = Clauses::new();
        assert!(
            short.push("Sure, I can do that for you now.").is_empty(),
            "cut a short sentence at its comma"
        );

        let mut long = Clauses::new();
        let ready = long.push(
            "I looked through the authentication module and the session handling code, \
             and then I checked the tests that cover both of them for you",
        );
        assert_eq!(ready.len(), 1, "{ready:?}");
        assert!(ready[0].ends_with(','), "{:?}", ready[0]);
    }

    /// The cuts are byte indices into text the model wrote, which is not all ASCII —
    /// landing one mid-character would panic rather than mispronounce.
    #[test]
    fn multi_byte_text_survives_intact() {
        let spoken = stream(&["Café renové, naïve façade. ", "Done — 100% ready."]);
        assert_eq!(spoken, vec!["Café renové, naïve façade.", "Done — 100% ready."]);
    }

    #[test]
    fn flushing_twice_yields_nothing_the_second_time() {
        let mut clauses = Clauses::new();
        clauses.push("Anything at all");
        assert!(clauses.flush().is_some());
        assert_eq!(clauses.flush(), None);
    }
}
