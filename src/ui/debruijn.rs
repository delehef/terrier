use anyhow::ensure;
use compact_str::CompactString;

pub struct DeBruijner {
    alphabet: Vec<char>,
}

impl Default for DeBruijner {
    fn default() -> Self {
        Self {
            alphabet: vec!['a', 's', 'd', 'f'],
        }
    }
}

impl DeBruijner {
    fn new(letters: String) -> anyhow::Result<Self> {
        let mut alphabet = letters.chars().collect::<Vec<char>>();
        alphabet.dedup();
        ensure!(
            alphabet.len() > 1,
            "at least two keys are required for keychords"
        );

        Ok(Self { alphabet })
    }

    fn debruijn(&self, n: usize) -> Vec<char> {
        fn db(seq: &mut Vec<u8>, a: &mut Vec<u8>, k: usize, n: usize, t: usize, p: usize) {
            if t > n {
                if n % p == 0 {
                    seq.append(&mut a[1..p + 1].to_vec().clone());
                }
            } else {
                a[t] = a[t - p];
                db(seq, a, k, n, t + 1, p);
                for j in (a[t - p] as usize + 1)..k {
                    a[t] = j as u8;
                    db(seq, a, k, n, t + 1, t);
                }
            }
        }

        let k = self.alphabet.len();
        let mut a = vec![0_u8; k * n];

        let mut seq: Vec<u8> = vec![];
        db(&mut seq, &mut a, k, n, 1, 1);
        seq.iter()
            .map(|i| self.alphabet[*i as usize])
            .collect::<Vec<char>>()
    }

    pub fn generate_n(&self, n: usize) -> Vec<CompactString> {
        if n == 0 {
            return Vec::new();
        }

        // For n < alphabet.size, log.ceil == 0
        let length = ((n as f32).log(4.0).ceil() as usize).max(1);
        let s = self.debruijn(length);

        (0..n)
            .map(|i| {
                s.iter()
                    .rev()
                    .chain(s.iter().rev())
                    .skip(i)
                    .take(length)
                    .collect::<CompactString>()
            })
            .collect()
    }
}
