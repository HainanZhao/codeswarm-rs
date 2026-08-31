use std::time::Instant;

use codeswarm::transcript::{BlockKind, Transcript, fixtures};

fn main() {
    let mut single = Transcript::default();
    single.append(
        BlockKind::Agent,
        fixtures::five_thousand_word_reply(),
        false,
    );

    let start = Instant::now();
    let rows = single.row_count(80);
    let warmup = start.elapsed();

    let start = Instant::now();
    let mut rendered_rows = 0;
    for scroll_y in (0..rows).step_by(3) {
        rendered_rows += single.viewport(80, scroll_y, 24, 8).len();
    }
    let scroll = start.elapsed();

    let mut long = fixtures::hundred_turn_transcript();
    let start = Instant::now();
    let long_rows = long.row_count(80);
    let long_layout = start.elapsed();

    println!(
        "scenario=single_5k width=80 rows={rows} warmup_ms={} scroll_ms={} rendered_rows={rendered_rows}",
        warmup.as_millis(),
        scroll.as_millis(),
    );
    println!(
        "scenario=hundred_turns width=80 rows={long_rows} layout_ms={}",
        long_layout.as_millis(),
    );
}
