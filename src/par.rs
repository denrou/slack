//! Run one call over a list on a few threads at once, keeping the order.
//! A cold name cache or a dozen conversations to read would otherwise be a
//! dozen sequential round trips to Slack.

use std::thread;

const WORKERS: usize = 8;

pub fn map<T, R>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    if items.len() <= 1 {
        return items.into_iter().map(f).collect();
    }
    let mut groups: Vec<Vec<(usize, T)>> = (0..WORKERS).map(|_| Vec::new()).collect();
    for (index, item) in items.into_iter().enumerate() {
        groups[index % WORKERS].push((index, item));
    }
    let f = &f;
    let mut results: Vec<(usize, R)> = thread::scope(|scope| {
        let handles: Vec<_> = groups
            .into_iter()
            .filter(|group| !group.is_empty())
            .map(|group| {
                scope.spawn(move || {
                    group
                        .into_iter()
                        .map(|(index, item)| (index, f(item)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a worker thread panicked"))
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_the_input_order() {
        let doubled = map((0..50).collect(), |n| n * 2);
        assert_eq!(doubled, (0..50).map(|n| n * 2).collect::<Vec<_>>());
        assert_eq!(map(vec![7], |n| n + 1), vec![8]);
        assert!(map(Vec::<u8>::new(), |n| n).is_empty());
    }
}
