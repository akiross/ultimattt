extern crate test;
use crate::game;

use rand;
use std::time::{Duration, Instant};

pub trait AI {
    fn select_move(&mut self, g: &game::Game) -> game::Move;
}

pub struct Minimax {
    #[allow(dead_code)]
    rng: rand::rngs::ThreadRng,

    max_depth: Option<i32>,
    timeout: Option<Duration>,
}

const EVAL_WON: i64 = 1 << 60;
const EVAL_LOST: i64 = -(1 << 60);
const EVAL_PARTIAL_ONE: i64 = 1;
const EVAL_PARTIAL_TWO: i64 = 3;

const OVERALL_PARTIAL_WIN: i64 = 10;

impl Minimax {
    #[allow(dead_code)]
    pub fn with_depth(depth: i32) -> Self {
        Self {
            rng: rand::thread_rng(),
            max_depth: Some(depth),
            timeout: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            rng: rand::thread_rng(),
            max_depth: None,
            timeout: Some(timeout),
        }
    }

    fn score_board(&self, g: &game::Game, board: usize) -> i64 {
        if !g.game_states.in_play(board) {
            return 0;
        }
        struct WinPotentials {
            // (x, o)
            n1: (i64, i64),
            n2: (i64, i64),
        };
        let xbits = g.boards.xbits(board);
        let obits = g.boards.obits(board);
        let mut potentials = WinPotentials {
            n1: (0, 0),
            n2: (0, 0),
        };

        for mask in game::WIN_MASKS {
            let xs = xbits & mask;
            let os = obits & mask;

            // If neither player has a play, or both players have a
            // play, this is a dead line; assign no points
            if (xs == 0 && os == 0) || (xs != 0 && os != 0) {
                continue;
            }
            if xs != 0 {
                if xs.count_ones() == 1 {
                    potentials.n1.0 += 1;
                } else if xs.count_ones() == 2 {
                    potentials.n2.0 += 1;
                }
            } else {
                if os.count_ones() == 1 {
                    potentials.n1.1 += 1;
                } else if os.count_ones() == 2 {
                    potentials.n2.1 += 1;
                }
            }
        }
        let d1 = EVAL_PARTIAL_ONE * (potentials.n1.0 - potentials.n1.1);
        let d2 = EVAL_PARTIAL_TWO * (potentials.n2.0 - potentials.n2.1);
        return d1 + d2;
    }

    fn evaluate(&self, g: &game::Game) -> i64 {
        match g.game_state() {
            game::BoardState::Drawn => (),
            game::BoardState::InPlay => (),
            game::BoardState::Won(p) => return if p == g.player() { EVAL_WON } else { EVAL_LOST },
        };

        let mut board_scores: [i64; 9] = [0; 9];
        for board in 0..9 {
            board_scores[board] = self.score_board(g, board);
        }
        let mut score: i64 = 0;
        for (i, mask) in game::WIN_MASKS.iter().enumerate() {
            let xbits = g.game_states.xbits() & mask;
            let obits = g.game_states.obits() & mask;

            // If both players have a play, this is a dead line. Award
            // no points.
            if xbits != 0 && obits != 0 {
                continue;
            }
            score += (xbits.count_ones() as i64) * OVERALL_PARTIAL_WIN;
            score -= (obits.count_ones() as i64) * OVERALL_PARTIAL_WIN;

            for idx in game::WIN_PATTERNS[i].iter() {
                score += board_scores[*idx];
            }
        }

        if g.player() == game::Player::O {
            -score
        } else {
            score
        }
    }

    fn minimax(&mut self, g: &game::Game, depth: i32) -> (game::Move, i64) {
        if depth <= 0 {
            return (game::Move::none(), self.evaluate(g));
        }

        let mut best = (game::Move::none(), EVAL_LOST - 1);
        let moves = g.all_moves();
        for m in moves {
            let child = g.make_move(m).unwrap();
            let mut eval = self.minimax(&child, depth - 1);
            eval.1 *= -1;
            if eval.1 > best.1 {
                best = (m, eval.1)
            }
        }
        best
    }
}

impl AI for Minimax {
    fn select_move(&mut self, g: &game::Game) -> game::Move {
        let deadline: Option<Instant> = self.timeout.map(|t| Instant::now() + t);
        let mut depth = 0;
        let mut result: game::Move;
        loop {
            depth += 1;
            let t_before = Instant::now();
            let got = self.minimax(g, depth);
            let ply_duration = Instant::now().duration_since(t_before);
            result = got.0;
            println!(
                "minimax depth={} move={} v={} t={}.{:03}s",
                depth,
                got.0,
                got.1,
                ply_duration.as_secs(),
                ply_duration.subsec_millis(),
            );
            if self.max_depth.map(|d| depth >= d).unwrap_or(false) {
                break;
            }
            if deadline.map(|d| Instant::now() > d).unwrap_or(false) {
                break;
            }
        }
        return result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::Bencher;

    #[bench]
    fn bench_evaluate(b: &mut Bencher) {
        use std::hint::black_box;
        let g = game::Game::new();
        let ai = Minimax::with_depth(3);
        b.iter(|| ai.evaluate(black_box(&g)));
    }
}
