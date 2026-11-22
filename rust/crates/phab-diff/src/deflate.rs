//! PHP's `gzdeflate()`: zlib's raw DEFLATE at the default level, byte for
//! byte.
//!
//! Stored hunks must be the bytes PHP writes, and DEFLATE encoders differ
//! in their output, so this is zlib's encoder (`deflate.c` and `trees.c`
//! of zlib 1.3) for what PHP asks of it: level 6 (`deflate_slow()` with
//! lazy matching), a 32 KiB window, `MAX_MEM_LEVEL` (9) and the default
//! strategy, with the whole input and enough output space in one
//! `deflate(Z_FINISH)` call.
//!
//! The integers are zlib's: lengths, distances and codes bounded by the
//! format, so the casts between them never truncate, as in C.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::needless_range_loop,
    clippy::similar_names
)]

/// `MIN_MATCH`.
const MIN_MATCH: usize = 3;
/// `MAX_MATCH`.
const MAX_MATCH: usize = 258;
/// `w_size`: 2^15.
const W_SIZE: usize = 1 << 15;
const W_MASK: usize = W_SIZE - 1;
/// `hash_bits` is `memLevel + 7`.
const HASH_BITS: u32 = 16;
const HASH_SIZE: usize = 1 << HASH_BITS;
const HASH_MASK: u32 = (1 << HASH_BITS) - 1;
const HASH_SHIFT: u32 = HASH_BITS.div_ceil(3);
/// `lit_bufsize` is `1 << (memLevel + 6)`.
const LIT_BUFSIZE: usize = 1 << 15;
/// `sym_end` counts symbols here, not bytes.
const SYM_END: usize = LIT_BUFSIZE - 1;
const WINDOW_SIZE: usize = 2 * W_SIZE;
const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
const MAX_DIST: usize = W_SIZE - MIN_LOOKAHEAD;
const TOO_FAR: usize = 4096;
/// The level 6 entry of `configuration_table`.
const GOOD_MATCH: usize = 8;
const MAX_LAZY_MATCH: usize = 16;
const NICE_MATCH: usize = 128;
const MAX_CHAIN: usize = 128;

const LENGTH_CODES: usize = 29;
const LITERALS: usize = 256;
const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;
const D_CODES: usize = 30;
const BL_CODES: usize = 19;
const HEAP_SIZE: usize = 2 * L_CODES + 1;
const MAX_BITS: usize = 15;
const MAX_BL_BITS: usize = 7;
const END_BLOCK: usize = 256;
const REP_3_6: usize = 16;
const REPZ_3_10: usize = 17;
const REPZ_11_138: usize = 18;

const EXTRA_LBITS: [u32; LENGTH_CODES] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const EXTRA_DBITS: [u32; D_CODES] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const EXTRA_BLBITS: [u32; BL_CODES] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];
const BL_ORDER: [usize; BL_CODES] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// The tables `tr_static_init()` builds.
struct Tables {
    length_code: [u8; 256],
    dist_code: [u8; 512],
    base_length: [u32; LENGTH_CODES],
    base_dist: [u32; D_CODES],
    static_ltree: Tree,
    static_dtree: Tree,
}

/// A `ct_data` array: `fc` holds the frequency, then the code; `dl` the
/// parent node, then the length, as zlib's unions do.
#[derive(Clone)]
struct Tree {
    fc: Vec<u32>,
    dl: Vec<u32>,
}

impl Tree {
    fn new(size: usize) -> Self {
        Self {
            fc: vec![0; size],
            dl: vec![0; size],
        }
    }
}

fn bi_reverse(code: u32, len: u32) -> u32 {
    let mut code = code;
    let mut result = 0;
    for _ in 0..len {
        result = (result << 1) | (code & 1);
        code >>= 1;
    }
    result
}

/// `gen_codes()`.
fn gen_codes(tree: &mut Tree, max_code: usize, bl_count: &[u32; MAX_BITS + 1]) {
    let mut next_code = [0u32; MAX_BITS + 1];
    let mut code = 0u32;
    for bits in 1..=MAX_BITS {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code & 0xffff;
    }
    for n in 0..=max_code {
        let len = tree.dl[n] as usize;
        if len == 0 {
            continue;
        }
        tree.fc[n] = bi_reverse(next_code[len], len as u32);
        next_code[len] += 1;
    }
}

impl Tables {
    fn new() -> Self {
        let mut length_code = [0u8; 256];
        let mut base_length = [0u32; LENGTH_CODES];
        let mut length = 0usize;
        let mut code = 0usize;
        while code < LENGTH_CODES - 1 {
            base_length[code] = length as u32;
            for _ in 0..(1usize << EXTRA_LBITS[code]) {
                length_code[length] = code as u8;
                length += 1;
            }
            code += 1;
        }
        length_code[length - 1] = code as u8;
        let mut dist_code = [0u8; 512];
        let mut base_dist = [0u32; D_CODES];
        let mut dist = 0usize;
        code = 0;
        while code < 16 {
            base_dist[code] = dist as u32;
            for _ in 0..(1usize << EXTRA_DBITS[code]) {
                dist_code[dist] = code as u8;
                dist += 1;
            }
            code += 1;
        }
        dist >>= 7;
        while code < D_CODES {
            base_dist[code] = (dist << 7) as u32;
            for _ in 0..(1usize << (EXTRA_DBITS[code] - 7)) {
                dist_code[256 + dist] = code as u8;
                dist += 1;
            }
            code += 1;
        }
        let mut static_ltree = Tree::new(L_CODES + 2);
        let mut bl_count = [0u32; MAX_BITS + 1];
        for n in 0..=L_CODES + 1 {
            let len = match n {
                0..=143 | 280..=287 => 8,
                144..=255 => 9,
                _ => 7,
            };
            static_ltree.dl[n] = len;
            bl_count[len as usize] += 1;
        }
        gen_codes(&mut static_ltree, L_CODES + 1, &bl_count);
        let mut static_dtree = Tree::new(D_CODES);
        for n in 0..D_CODES {
            static_dtree.dl[n] = 5;
            static_dtree.fc[n] = bi_reverse(n as u32, 5);
        }
        Self {
            length_code,
            dist_code,
            base_length,
            base_dist,
            static_ltree,
            static_dtree,
        }
    }

    fn d_code(&self, dist: usize) -> usize {
        if dist < 256 {
            usize::from(self.dist_code[dist])
        } else {
            usize::from(self.dist_code[256 + (dist >> 7)])
        }
    }
}

/// Which tree a `tree_desc` describes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Literal,
    Distance,
    BitLength,
}

/// The bits written so far, least significant first.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    buffer: u64,
    valid: u32,
}

impl Bits {
    fn send(&mut self, value: u32, length: u32) {
        self.buffer |= u64::from(value) << self.valid;
        self.valid += length;
        while self.valid >= 8 {
            self.out.push((self.buffer & 0xff) as u8);
            self.buffer >>= 8;
            self.valid -= 8;
        }
    }

    /// `bi_windup()`.
    fn windup(&mut self) {
        if self.valid > 0 {
            self.out.push((self.buffer & 0xff) as u8);
        }
        self.buffer = 0;
        self.valid = 0;
    }
}

/// `deflate_state`, for one call.
struct State<'a> {
    tables: Tables,
    input: &'a [u8],
    next_in: usize,
    window: Vec<u8>,
    prev: Vec<u16>,
    head: Vec<u16>,
    ins_h: u32,
    block_start: i64,
    match_length: usize,
    prev_match: usize,
    match_available: bool,
    strstart: usize,
    match_start: usize,
    lookahead: usize,
    prev_length: usize,
    dyn_ltree: Tree,
    dyn_dtree: Tree,
    bl_tree: Tree,
    l_max_code: usize,
    d_max_code: usize,
    bl_count: [u32; MAX_BITS + 1],
    heap: [usize; HEAP_SIZE],
    heap_len: usize,
    heap_max: usize,
    depth: [u8; HEAP_SIZE],
    /// `sym_buf`: distance (0 for a literal) and literal or length.
    symbols: Vec<(u16, u8)>,
    opt_len: i64,
    static_len: i64,
    bits: Bits,
}

impl<'a> State<'a> {
    fn new(input: &'a [u8]) -> Self {
        let mut state = Self {
            tables: Tables::new(),
            input,
            next_in: 0,
            // zlib's window is exactly this big; the slack keeps reads past
            // the end (which zlib also makes, of zeroed bytes) in bounds.
            window: vec![0; WINDOW_SIZE + MAX_MATCH + 8],
            prev: vec![0; W_SIZE],
            head: vec![0; HASH_SIZE],
            ins_h: 0,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            dyn_ltree: Tree::new(HEAP_SIZE),
            dyn_dtree: Tree::new(2 * D_CODES + 1),
            bl_tree: Tree::new(2 * BL_CODES + 1),
            l_max_code: 0,
            d_max_code: 0,
            bl_count: [0; MAX_BITS + 1],
            heap: [0; HEAP_SIZE],
            heap_len: 0,
            heap_max: 0,
            depth: [0; HEAP_SIZE],
            symbols: Vec::with_capacity(SYM_END),
            opt_len: 0,
            static_len: 0,
            bits: Bits::default(),
        };
        state.init_block();
        state
    }

    fn update_hash(&mut self, byte: u8) {
        self.ins_h = ((self.ins_h << HASH_SHIFT) ^ u32::from(byte)) & HASH_MASK;
    }

    /// `INSERT_STRING()`: returns the previous head of the chain.
    fn insert_string(&mut self, position: usize) -> usize {
        self.update_hash(self.window[position + MIN_MATCH - 1]);
        let head = self.head[self.ins_h as usize];
        self.prev[position & W_MASK] = head;
        self.head[self.ins_h as usize] = position as u16;
        usize::from(head)
    }

    /// `slide_hash()`.
    fn slide_hash(&mut self) {
        for entry in self.head.iter_mut().chain(self.prev.iter_mut()) {
            let value = usize::from(*entry);
            *entry = if value >= W_SIZE {
                (value - W_SIZE) as u16
            } else {
                0
            };
        }
    }

    /// `fill_window()`; `insert` is always 0 within one call.
    fn fill_window(&mut self) {
        loop {
            let mut more = WINDOW_SIZE - self.lookahead - self.strstart;
            if self.strstart >= W_SIZE + MAX_DIST {
                self.window.copy_within(W_SIZE..2 * W_SIZE - more, 0);
                self.match_start = self.match_start.wrapping_sub(W_SIZE);
                self.strstart -= W_SIZE;
                self.block_start -= W_SIZE as i64;
                self.slide_hash();
                more += W_SIZE;
            }
            if self.next_in == self.input.len() {
                break;
            }
            let count = more.min(self.input.len() - self.next_in);
            let at = self.strstart + self.lookahead;
            self.window[at..at + count]
                .copy_from_slice(&self.input[self.next_in..self.next_in + count]);
            self.next_in += count;
            self.lookahead += count;
            if self.lookahead >= MIN_MATCH {
                let position = self.strstart;
                self.ins_h = u32::from(self.window[position]);
                self.update_hash(self.window[position + 1]);
            }
            if self.lookahead >= MIN_LOOKAHEAD || self.next_in == self.input.len() {
                break;
            }
        }
    }

    /// `longest_match()`.
    fn longest_match(&mut self, cur_match: usize) -> usize {
        let mut cur_match = cur_match;
        let mut chain_length = MAX_CHAIN;
        let scan = self.strstart;
        let mut best_len = self.prev_length;
        let mut nice_match = NICE_MATCH;
        let limit = self.strstart.saturating_sub(MAX_DIST);
        let strend = scan + MAX_MATCH;
        let window = &self.window;
        let mut scan_end1 = window[scan + best_len - 1];
        let mut scan_end = window[scan + best_len];
        if self.prev_length >= GOOD_MATCH {
            chain_length >>= 2;
        }
        if nice_match > self.lookahead {
            nice_match = self.lookahead;
        }
        loop {
            let candidate = cur_match;
            if window[candidate + best_len] == scan_end
                && window[candidate + best_len - 1] == scan_end1
                && window[candidate] == window[scan]
                && window[candidate + 1] == window[scan + 1]
            {
                // scan[2] and match[2] are equal when the hashes are.
                let mut s = scan + 2;
                let mut m = candidate + 2;
                'compare: loop {
                    for _ in 0..8 {
                        s += 1;
                        m += 1;
                        if window[s] != window[m] {
                            break 'compare;
                        }
                    }
                    if s >= strend {
                        break;
                    }
                }
                let len = MAX_MATCH - (strend - s);
                if len > best_len {
                    self.match_start = candidate;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = window[scan + best_len - 1];
                    scan_end = window[scan + best_len];
                }
            }
            cur_match = usize::from(self.prev[cur_match & W_MASK]);
            if cur_match <= limit {
                break;
            }
            chain_length -= 1;
            if chain_length == 0 {
                break;
            }
        }
        best_len.min(self.lookahead)
    }

    /// `_tr_tally_lit()`: whether the block is full.
    fn tally_lit(&mut self, byte: u8) -> bool {
        self.symbols.push((0, byte));
        self.dyn_ltree.fc[usize::from(byte)] += 1;
        self.symbols.len() == SYM_END
    }

    /// `_tr_tally_dist()`: whether the block is full.
    fn tally_dist(&mut self, distance: usize, length: usize) -> bool {
        self.symbols.push((distance as u16, length as u8));
        let code = usize::from(self.tables.length_code[length]);
        self.dyn_ltree.fc[code + LITERALS + 1] += 1;
        let dcode = self.tables.d_code(distance - 1);
        self.dyn_dtree.fc[dcode] += 1;
        self.symbols.len() == SYM_END
    }

    /// `FLUSH_BLOCK_ONLY()`.
    fn flush_block(&mut self, last: bool) {
        let stored_len = (self.strstart as i64 - self.block_start) as usize;
        let buffer = usize::try_from(self.block_start).ok();
        self.tr_flush_block(buffer, stored_len, last);
        self.block_start = self.strstart as i64;
    }

    /// `deflate_slow()` with `Z_FINISH`.
    fn deflate_slow(&mut self) {
        loop {
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window();
                if self.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;
            if hash_head != 0
                && self.prev_length < MAX_LAZY_MATCH
                && self.strstart - hash_head <= MAX_DIST
            {
                self.match_length = self.longest_match(hash_head);
                if self.match_length <= 5
                    && self.match_length == MIN_MATCH
                    && self.strstart - self.match_start > TOO_FAR
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }
            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                let max_insert = self.strstart + self.lookahead - MIN_MATCH;
                let full = self.tally_dist(
                    self.strstart - 1 - self.prev_match,
                    self.prev_length - MIN_MATCH,
                );
                self.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.strstart += 1;
                    if self.strstart <= max_insert {
                        self.insert_string(self.strstart);
                    }
                    self.prev_length -= 1;
                    if self.prev_length == 0 {
                        break;
                    }
                }
                self.match_available = false;
                self.match_length = MIN_MATCH - 1;
                self.strstart += 1;
                if full {
                    self.flush_block(false);
                }
            } else if self.match_available {
                let full = self.tally_lit(self.window[self.strstart - 1]);
                if full {
                    self.flush_block(false);
                }
                self.strstart += 1;
                self.lookahead -= 1;
            } else {
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }
        if self.match_available {
            self.tally_lit(self.window[self.strstart - 1]);
            self.match_available = false;
        }
        self.flush_block(true);
    }

    /// `init_block()`.
    fn init_block(&mut self) {
        self.dyn_ltree.fc[..L_CODES].fill(0);
        self.dyn_dtree.fc[..D_CODES].fill(0);
        self.bl_tree.fc[..BL_CODES].fill(0);
        self.dyn_ltree.fc[END_BLOCK] = 1;
        self.opt_len = 0;
        self.static_len = 0;
        self.symbols.clear();
    }

    fn tree_mut(&mut self, kind: Kind) -> &mut Tree {
        match kind {
            Kind::Literal => &mut self.dyn_ltree,
            Kind::Distance => &mut self.dyn_dtree,
            Kind::BitLength => &mut self.bl_tree,
        }
    }

    /// `smaller()`.
    fn smaller(&self, tree: &Tree, n: usize, m: usize) -> bool {
        tree.fc[n] < tree.fc[m] || (tree.fc[n] == tree.fc[m] && self.depth[n] <= self.depth[m])
    }

    /// `pqdownheap()`.
    fn pqdownheap(&mut self, tree: &Tree, k: usize) {
        let mut k = k;
        let v = self.heap[k];
        let mut j = k << 1;
        while j <= self.heap_len {
            if j < self.heap_len && self.smaller(tree, self.heap[j + 1], self.heap[j]) {
                j += 1;
            }
            if self.smaller(tree, v, self.heap[j]) {
                break;
            }
            self.heap[k] = self.heap[j];
            k = j;
            j <<= 1;
        }
        self.heap[k] = v;
    }

    /// `build_tree()`: returns `max_code`.
    fn build_tree(&mut self, kind: Kind) -> usize {
        let mut tree = std::mem::replace(self.tree_mut(kind), Tree::new(0));
        let (elems, has_static) = match kind {
            Kind::Literal => (L_CODES, true),
            Kind::Distance => (D_CODES, true),
            Kind::BitLength => (BL_CODES, false),
        };
        let mut max_code: i64 = -1;
        self.heap_len = 0;
        self.heap_max = HEAP_SIZE;
        for n in 0..elems {
            if tree.fc[n] == 0 {
                tree.dl[n] = 0;
            } else {
                self.heap_len += 1;
                self.heap[self.heap_len] = n;
                max_code = n as i64;
                self.depth[n] = 0;
            }
        }
        while self.heap_len < 2 {
            let node = if max_code < 2 {
                max_code += 1;
                max_code as usize
            } else {
                0
            };
            self.heap_len += 1;
            self.heap[self.heap_len] = node;
            tree.fc[node] = 1;
            self.depth[node] = 0;
            self.opt_len -= 1;
            if has_static {
                self.static_len -= i64::from(self.static_tree(kind).dl[node]);
            }
        }
        let max_code = max_code as usize;
        for n in (1..=self.heap_len / 2).rev() {
            self.pqdownheap(&tree, n);
        }
        let mut node = elems;
        loop {
            let n = self.heap[1];
            self.heap[1] = self.heap[self.heap_len];
            self.heap_len -= 1;
            self.pqdownheap(&tree, 1);
            let m = self.heap[1];
            self.heap_max -= 1;
            self.heap[self.heap_max] = n;
            self.heap_max -= 1;
            self.heap[self.heap_max] = m;
            tree.fc[node] = tree.fc[n] + tree.fc[m];
            self.depth[node] = self.depth[n].max(self.depth[m]) + 1;
            tree.dl[n] = node as u32;
            tree.dl[m] = node as u32;
            self.heap[1] = node;
            node += 1;
            self.pqdownheap(&tree, 1);
            if self.heap_len < 2 {
                break;
            }
        }
        self.heap_max -= 1;
        self.heap[self.heap_max] = self.heap[1];
        self.gen_bitlen(&mut tree, kind, max_code);
        let bl_count = self.bl_count;
        gen_codes(&mut tree, max_code, &bl_count);
        *self.tree_mut(kind) = tree;
        max_code
    }

    fn static_tree(&self, kind: Kind) -> &Tree {
        match kind {
            Kind::Distance => &self.tables.static_dtree,
            _ => &self.tables.static_ltree,
        }
    }

    /// `gen_bitlen()`.
    fn gen_bitlen(&mut self, tree: &mut Tree, kind: Kind, max_code: usize) {
        let (extra, base, max_length): (&[u32], usize, usize) = match kind {
            Kind::Literal => (&EXTRA_LBITS, LITERALS + 1, MAX_BITS),
            Kind::Distance => (&EXTRA_DBITS, 0, MAX_BITS),
            Kind::BitLength => (&EXTRA_BLBITS, 0, MAX_BL_BITS),
        };
        let has_static = kind != Kind::BitLength;
        self.bl_count = [0; MAX_BITS + 1];
        let mut overflow: i64 = 0;
        tree.dl[self.heap[self.heap_max]] = 0;
        let mut h = self.heap_max + 1;
        while h < HEAP_SIZE {
            let n = self.heap[h];
            let mut bits = tree.dl[tree.dl[n] as usize] as usize + 1;
            if bits > max_length {
                bits = max_length;
                overflow += 1;
            }
            tree.dl[n] = bits as u32;
            h += 1;
            if n > max_code {
                continue;
            }
            self.bl_count[bits] += 1;
            let xbits = if n >= base { extra[n - base] } else { 0 };
            let frequency = i64::from(tree.fc[n]);
            self.opt_len += frequency * (bits as i64 + i64::from(xbits));
            if has_static {
                let static_len = i64::from(self.static_tree(kind).dl[n]);
                self.static_len += frequency * (static_len + i64::from(xbits));
            }
        }
        if overflow == 0 {
            return;
        }
        loop {
            let mut bits = max_length - 1;
            while self.bl_count[bits] == 0 {
                bits -= 1;
            }
            self.bl_count[bits] -= 1;
            self.bl_count[bits + 1] += 2;
            self.bl_count[max_length] -= 1;
            overflow -= 2;
            if overflow <= 0 {
                break;
            }
        }
        h = HEAP_SIZE;
        for bits in (1..=max_length).rev() {
            let mut n = self.bl_count[bits];
            while n != 0 {
                h -= 1;
                let m = self.heap[h];
                if m > max_code {
                    continue;
                }
                if tree.dl[m] as usize != bits {
                    self.opt_len += (bits as i64 - i64::from(tree.dl[m])) * i64::from(tree.fc[m]);
                    tree.dl[m] = bits as u32;
                }
                n -= 1;
            }
        }
    }

    /// `scan_tree()`.
    fn scan_tree(&mut self, kind: Kind, max_code: usize) {
        let tree = self.tree_mut(kind);
        tree.dl[max_code + 1] = 0xffff;
        let lengths: Vec<u32> = tree.dl[..=max_code + 1].to_vec();
        let mut prevlen: i64 = -1;
        let mut nextlen = lengths[0];
        let mut count = 0;
        let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
        for n in 0..=max_code {
            let curlen = nextlen;
            nextlen = lengths[n + 1];
            count += 1;
            if count < max_count && curlen == nextlen {
                continue;
            }
            if count < min_count {
                self.bl_tree.fc[curlen as usize] += count;
            } else if curlen != 0 {
                if i64::from(curlen) != prevlen {
                    self.bl_tree.fc[curlen as usize] += 1;
                }
                self.bl_tree.fc[REP_3_6] += 1;
            } else if count <= 10 {
                self.bl_tree.fc[REPZ_3_10] += 1;
            } else {
                self.bl_tree.fc[REPZ_11_138] += 1;
            }
            count = 0;
            prevlen = i64::from(curlen);
            (max_count, min_count) = if nextlen == 0 {
                (138, 3)
            } else if curlen == nextlen {
                (6, 3)
            } else {
                (7, 4)
            };
        }
    }

    fn send_code(&mut self, code: usize, tree: Kind) {
        let (value, length) = {
            let tree = match tree {
                Kind::Literal => &self.dyn_ltree,
                Kind::Distance => &self.dyn_dtree,
                Kind::BitLength => &self.bl_tree,
            };
            (tree.fc[code], tree.dl[code])
        };
        self.bits.send(value, length);
    }

    /// `send_tree()`.
    fn send_tree(&mut self, kind: Kind, max_code: usize) {
        let lengths: Vec<u32> = self.tree_mut(kind).dl[..=max_code + 1].to_vec();
        let mut prevlen: i64 = -1;
        let mut nextlen = lengths[0];
        let mut count: u32 = 0;
        let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
        for n in 0..=max_code {
            let curlen = nextlen;
            nextlen = lengths[n + 1];
            count += 1;
            if count < max_count && curlen == nextlen {
                continue;
            }
            if count < min_count {
                while count != 0 {
                    self.send_code(curlen as usize, Kind::BitLength);
                    count -= 1;
                }
            } else if curlen != 0 {
                if i64::from(curlen) != prevlen {
                    self.send_code(curlen as usize, Kind::BitLength);
                    count -= 1;
                }
                self.send_code(REP_3_6, Kind::BitLength);
                self.bits.send(count - 3, 2);
            } else if count <= 10 {
                self.send_code(REPZ_3_10, Kind::BitLength);
                self.bits.send(count - 3, 3);
            } else {
                self.send_code(REPZ_11_138, Kind::BitLength);
                self.bits.send(count - 11, 7);
            }
            count = 0;
            prevlen = i64::from(curlen);
            (max_count, min_count) = if nextlen == 0 {
                (138, 3)
            } else if curlen == nextlen {
                (6, 3)
            } else {
                (7, 4)
            };
        }
    }

    /// `build_bl_tree()`: returns `max_blindex`.
    fn build_bl_tree(&mut self) -> usize {
        self.scan_tree(Kind::Literal, self.l_max_code);
        self.scan_tree(Kind::Distance, self.d_max_code);
        self.build_tree(Kind::BitLength);
        let mut max_blindex = BL_CODES - 1;
        while max_blindex >= 3 {
            if self.bl_tree.dl[BL_ORDER[max_blindex]] != 0 {
                break;
            }
            max_blindex -= 1;
        }
        self.opt_len += 3 * (max_blindex as i64 + 1) + 5 + 5 + 4;
        max_blindex
    }

    /// `send_all_trees()`.
    fn send_all_trees(&mut self, lcodes: usize, dcodes: usize, blcodes: usize) {
        self.bits.send((lcodes - 257) as u32, 5);
        self.bits.send((dcodes - 1) as u32, 5);
        self.bits.send((blcodes - 4) as u32, 4);
        for &order in &BL_ORDER[..blcodes] {
            let length = self.bl_tree.dl[order];
            self.bits.send(length, 3);
        }
        self.send_tree(Kind::Literal, lcodes - 1);
        self.send_tree(Kind::Distance, dcodes - 1);
    }

    /// `compress_block()`, with the dynamic trees or the static ones.
    fn compress_block(&mut self, dynamic: bool) {
        let symbols = std::mem::take(&mut self.symbols);
        let (ltree, dtree) = if dynamic {
            (&self.dyn_ltree, &self.dyn_dtree)
        } else {
            (&self.tables.static_ltree, &self.tables.static_dtree)
        };
        for &(distance, lc) in &symbols {
            let lc = usize::from(lc);
            if distance == 0 {
                self.bits.send(ltree.fc[lc], ltree.dl[lc]);
                continue;
            }
            let code = usize::from(self.tables.length_code[lc]);
            let symbol = code + LITERALS + 1;
            self.bits.send(ltree.fc[symbol], ltree.dl[symbol]);
            let extra = EXTRA_LBITS[code];
            if extra != 0 {
                self.bits
                    .send(lc as u32 - self.tables.base_length[code], extra);
            }
            let dist = usize::from(distance) - 1;
            let code = self.tables.d_code(dist);
            self.bits.send(dtree.fc[code], dtree.dl[code]);
            let extra = EXTRA_DBITS[code];
            if extra != 0 {
                self.bits
                    .send(dist as u32 - self.tables.base_dist[code], extra);
            }
        }
        self.bits.send(ltree.fc[END_BLOCK], ltree.dl[END_BLOCK]);
        self.symbols = symbols;
    }

    /// `_tr_flush_block()`.
    fn tr_flush_block(&mut self, buffer: Option<usize>, stored_len: usize, last: bool) {
        self.l_max_code = self.build_tree(Kind::Literal);
        self.d_max_code = self.build_tree(Kind::Distance);
        let max_blindex = self.build_bl_tree();
        let mut opt_lenb = (self.opt_len + 3 + 7) >> 3;
        let static_lenb = (self.static_len + 3 + 7) >> 3;
        if static_lenb <= opt_lenb {
            opt_lenb = static_lenb;
        }
        let last_bit = u32::from(last);
        match buffer {
            Some(start) if stored_len as i64 + 4 <= opt_lenb => {
                self.bits.send(last_bit, 3);
                self.bits.windup();
                let length = stored_len as u16;
                self.bits.out.extend_from_slice(&length.to_le_bytes());
                self.bits.out.extend_from_slice(&(!length).to_le_bytes());
                self.bits
                    .out
                    .extend_from_slice(&self.window[start..start + stored_len]);
            }
            _ if static_lenb == opt_lenb => {
                self.bits.send((1 << 1) + last_bit, 3);
                self.compress_block(false);
            }
            _ => {
                self.bits.send((2 << 1) + last_bit, 3);
                self.send_all_trees(self.l_max_code + 1, self.d_max_code + 1, max_blindex + 1);
                self.compress_block(true);
            }
        }
        self.init_block();
        if last {
            self.bits.windup();
        }
    }
}

/// `gzdeflate($data)`.
pub fn gzdeflate(data: &[u8]) -> Vec<u8> {
    let mut state = State::new(data);
    state.deflate_slow();
    state.bits.out
}

/// `PhabricatorCaches::maybeDeflateData()`: the deflated data when the
/// data is over 1 KiB and deflates to less than half its size.
pub fn maybe_deflate(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() <= 1024 {
        return None;
    }
    let deflated = gzdeflate(data);
    // `$deflated_len >= ($len / 2)`, with PHP's float division.
    (deflated.len() * 2 < data.len()).then_some(deflated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::inflate;

    #[test]
    fn round_trips() {
        let mut text = Vec::new();
        for n in 0..5000 {
            text.extend_from_slice(format!("+line {n} of the hunk {}\n", n % 17).as_bytes());
        }
        for data in [&b""[..], b"a", b"abcabcabcabcabc", &text] {
            assert_eq!(inflate(&gzdeflate(data)).unwrap(), data);
        }
    }

    #[test]
    fn matches_zlib() {
        // php -r 'echo bin2hex(gzdeflate(str_repeat("hello world\n", 3) . "bye"));'
        assert_eq!(
            hex(&gzdeflate(b"hello world\nhello world\nhello world\nbye")),
            "cb48cdc9c95728cf2fca49e1cac0c14eaa4c0500"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .fold(String::new(), |hex, byte| hex + &format!("{byte:02x}"))
    }
}
