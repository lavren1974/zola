use core::time;
use std::{collections::HashMap, path::PathBuf, thread};

use libs::rayon::prelude::*;

use crate::{anyhow, Site};
use errors::{bail, Result};
use libs::rayon;
use libs::url::Url;

/// Check whether all internal links pointing to explicit anchor fragments are valid.
///
/// This is very similar to `check_external_links`, although internal links checking
/// is always performed (while external ones only conditionally in `zola check`).
pub fn check_internal_links_with_anchors(site: &Site) -> Result<()> {
    println!("Checking all internal links with anchors.");
    let library = site.library.write().expect("Get lock for check_internal_links_with_anchors");

    // Chain all internal links, from both sections and pages.
    let page_links = library.pages().values().flat_map(|p| {
        let path = &p.file.path;
        p.internal_links.iter().map(move |l| (path.clone(), l))
    });
    let section_links = library.sections().values().flat_map(|p| {
        let path = &p.file.path;
        p.internal_links.iter().map(move |l| (path.clone(), l))
    });
    let all_links = page_links.chain(section_links);

    // Only keep links with anchor fragments, and count them too.
    // Bare files have already been checked elsewhere, thus they are not interesting here.
    let mut anchors_total = 0usize;
    let links_with_anchors = all_links
        .filter_map(|(page_path, link)| match link {
            (md_path, Some(anchor)) => Some((page_path, md_path, anchor)),
            _ => None,
        })
        .inspect(|_| anchors_total = anchors_total.saturating_add(1));

    // Check for targets existence (including anchors), then keep only the faulty
    // entries for error reporting purposes.
    let missing_targets = links_with_anchors.filter(|(page, md_path, anchor)| {
        // There are a few `expect` here since the presence of the .md file will
        // already have been checked in the markdown rendering
        let mut full_path = site.base_path.clone();
        full_path.push("content");
        for part in md_path.split('/') {
            full_path.push(part);
        }
        // NOTE: This will also match _index.foobar.md where foobar is not a language
        // as well as any other sring containing "_index." which is now referenced as
        // unsupported page path in the docs.
        if md_path.contains("_index.") {
            let section = library.get_section(&full_path).expect(&format!(
                "Couldn't find section {} in check_internal_links_with_anchors from page {:?}",
                md_path,
                page.strip_prefix(&site.base_path).unwrap()
            ));
            !section.has_anchor(anchor)
        } else {
            let page = library.get_page(&full_path).expect(&format!(
                "Couldn't find page {} in check_internal_links_with_anchors from page {:?}",
                md_path,
                page.strip_prefix(&site.base_path).unwrap()
            ));

            !(page.has_anchor(anchor) || page.has_anchor_id(anchor))
        }
    });

    // Format faulty entries into error messages, and collect them.
    let errors = missing_targets
        .map(|(page_path, md_path, anchor)| {
            format!(
                "The anchor in the link `@/{}#{}` in {} does not exist.",
                md_path,
                anchor,
                page_path.to_string_lossy(),
            )
        })
        .collect::<Vec<_>>();

    // Finally emit a summary, and return overall anchors-checking result.
    match errors.len() {
        0 => {
            println!("> Successfully checked {} internal link(s) with anchors.", anchors_total);
            Ok(())
        }
        errors_total => {
            println!(
                "> Checked {} internal link(s) with anchors: {} target(s) missing.",
                anchors_total, errors_total,
            );
            Err(anyhow!(errors.join("\n")))
        }
    }
}

fn should_skip_by_prefix(link: &String, skip_prefixes: &Vec<String>) -> bool {
    skip_prefixes.iter().any(|prefix| link.starts_with(prefix))
}

fn get_link_domain(link: &str) -> Result<String> {
    return match Url::parse(link) {
        Ok(url) => match url.host_str().map(String::from) {
            Some(domain_str) => Ok(domain_str),
            None => bail!("could not parse domain `{}` from link", link),
        },
        Err(err) => bail!("could not parse domain `{}` from link: `{}`", link, err),
    };
}

pub fn check_external_links(site: &Site) -> Result<()> {
    let library = site.library.write().expect("Get lock for check_external_links");

    struct LinkDef {
        file_path: PathBuf,
        external_link: String,
        domain: String,
    }

    impl LinkDef {
        pub fn new(file_path: PathBuf, external_link: String, domain: String) -> Self {
            Self { file_path, external_link, domain }
        }
    }

    let mut checked_links: Vec<LinkDef> = vec![];
    let mut skipped_link_count: u32 = 0;

    for p in library.pages_values().into_iter() {
        for external_link in p.clone().external_links.into_iter() {
            if should_skip_by_prefix(&external_link, &site.config.link_checker.skip_prefixes) {
                skipped_link_count += 1;
            } else {
                let domain = get_link_domain(&external_link)?;
                checked_links.push(LinkDef::new(p.file.path.clone(), external_link, domain));
            }
        }
    }

    for s in library.sections_values().into_iter() {
        for external_link in s.clone().external_links.into_iter() {
            if should_skip_by_prefix(&external_link, &site.config.link_checker.skip_prefixes) {
                skipped_link_count += 1;
            } else {
                let domain = get_link_domain(&external_link)?;
                checked_links.push(LinkDef::new(s.file.path.clone(), external_link, domain));
            }
        }
    }

    println!(
        "Checking {} external link(s).  Skipping {} external link(s).",
        checked_links.len(),
        skipped_link_count
    );

    let mut links_by_domain: HashMap<String, Vec<&LinkDef>> = HashMap::new();

    for link in checked_links.iter() {
        links_by_domain.entry(link.domain.to_string()).or_default();
        // Insert content path and link under the domain key
        links_by_domain.get_mut(&link.domain).unwrap().push(&link);
    }

    if checked_links.is_empty() {
        return Ok(());
    }

    // create thread pool with lots of threads so we can fetch
    // (almost) all pages simultaneously, limiting all links for a single
    // domain to one thread to avoid rate-limiting
    let threads = std::cmp::min(links_by_domain.len(), 8);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;

    let errors = pool.install(|| {
        links_by_domain
            .par_iter()
            .map(|(_domain, links)| {
                let mut links_to_process = links.len();
                links
                    .iter()
                    .filter_map(move |link_def| {
                        links_to_process -= 1;

                        let res = link_checker::check_url(
                            &link_def.external_link,
                            &site.config.link_checker,
                        );

                        if links_to_process > 0 {
                            // Prevent rate-limiting, wait before next crawl unless we're done with this domain
                            thread::sleep(time::Duration::from_millis(500));
                        }

                        if link_checker::is_valid(&res) {
                            None
                        } else {
                            Some((&link_def.file_path, &link_def.external_link, res))
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect::<Vec<_>>()
    });

    println!(
        "> Checked {} external link(s): {} error(s) found.",
        checked_links.len(),
        errors.len()
    );

    if errors.is_empty() {
        return Ok(());
    }

    let msg = errors
        .into_iter()
        .map(|(page_path, link, check_res)| {
            format!(
                "Dead link in {} to {}: {}",
                page_path.to_string_lossy(),
                link,
                link_checker::message(&check_res)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Err(anyhow!(msg))
}
