extern crate clap;
extern crate fs_extra;

use clap::{App, Arg};
use futures::stream::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use scraper::Html;
use scraper::Selector;
use serde::Deserialize;
use std::error::Error;
use std::fs::File;
use std::io::prelude::*;
use std::path::PathBuf;
use tempfile::tempdir;

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);
lazy_static! {
    static ref CLIENT: reqwest::Client = reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()
        .unwrap();
    static ref ID_RE: Regex = Regex::new(r"volumeNo=(?P<vol>\d+)").unwrap();
}

#[derive(Debug)]
struct Volume {
    title: String,
    id: String,
}

impl PartialEq for Volume {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let matches = App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(
            Arg::with_name("DIRECTORY")
                .short("d")
                .long("directory")
                .value_name("DIR")
                .help("Directory to download to (default: ./posts)")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("MEMBER")
                .short("m")
                .long("member")
                .value_name("MEMBER_ID")
                .help("Set NP member id (default: 29156514)")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("URL")
                .short("u")
                .long("url")
                .value_name("URL")
                .multiple(true)
                .help("Download NP URL")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("FILTER")
                .short("f")
                .long("filter")
                .value_name("REGEX")
                .help("Regex filter on NP title")
                .takes_value(true),
        )
        .get_matches();

    let path = PathBuf::from(matches.value_of("DIRECTORY").unwrap_or("posts"));

    if matches.is_present("URL") {
        for url in matches.values_of("URL").unwrap() {
            process_one(url, &path).await?;
        }
    } else {
        let member = matches.value_of("MEMBER").unwrap_or("29156514");
        let filter = match matches.is_present("FILTER") {
            true => RegexBuilder::new(matches.value_of("FILTER").unwrap()),
            false => RegexBuilder::new(r".*"),
        }
        .case_insensitive(true)
        .build()
        .unwrap();

        process_member(member, &path, &filter).await?;
    }

    Ok(())
}

async fn process_one(url: &str, path: &PathBuf) -> Result<(), Box<dyn Error>> {
    let vol = ID_RE
        .captures_iter(&url)
        .filter_map(|c| c.name("vol"))
        .map(|m| m.as_str().to_owned())
        .next()
        .unwrap();
    download_np(&vol, &path).await?;
    Ok(())
}

async fn process_member(
    member: &str,
    path: &PathBuf,
    filter: &Regex,
) -> Result<(), Box<dyn Error>> {
    let mut page: usize = 1;
    let mut first = true;
    let mut num_found = 0;
    let mut np_vols: Vec<Volume> = vec![];

    println!("Retrieving IDs...");
    let pb = ProgressBar::new_spinner();
    let sty = ProgressStyle::default_bar()
        .template("Downloading page {pos:} {spinner}")
        .progress_chars("=> ");
    pb.set_style(sty);

    while first || num_found > 0 {
        pb.set_position(page as u64);
        first = false;
        let page_np_vols = get_ids(&member, page).await?;
        num_found = page_np_vols.len();
        np_vols.extend(page_np_vols);
        page += 1;
    }
    pb.finish_and_clear();
    np_vols.dedup();
    np_vols.retain(|vol| filter.is_match(&vol.title));

    println!("Downloading posts...");
    for vol in np_vols {
        download_np(vol.id.as_str(), &path).await?;
    }

    Ok(())
}

async fn get_ids(member: &str, page: usize) -> Result<Vec<Volume>, Box<dyn Error>> {
    lazy_static! {
        static ref SEL: Selector = Selector::parse("a.link_end").unwrap();
        static ref TITLE_SEL: Selector = Selector::parse(".tit_feed").unwrap();
        static ref ESCAPE_RE: Regex = Regex::new(r#"\\(?P<c>[^"n])"#).unwrap();
    }
    const URL: &str = "https://post.naver.com/async/my.nhn";

    #[derive(Deserialize)]
    struct HTML {
        html: String,
    }

    let text = CLIENT
        .get(URL)
        .query(&[
            ("memberNo", member.to_owned()),
            ("fromNo", page.to_string()),
        ])
        .send()
        .await?
        .text()
        .await?;
    let text = ESCAPE_RE.replace_all(&text, "$c");
    let body = serde_json::from_str::<HTML>(&text)?.html;

    let document = Html::parse_fragment(&body);
    let ret = document
        .select(&SEL)
        .filter_map(|e| {
            // get volume number
            let vol = ID_RE
                .captures_iter(&e.value().attr("href")?)
                .filter_map(|c| c.name("vol"))
                .map(|m| m.as_str().to_owned())
                .next()?;
            // try to get title
            let title = match Html::parse_fragment(&e.inner_html())
                .select(&TITLE_SEL)
                .next()
            {
                Some(v) => v.text().collect::<Vec<_>>().join(""),
                None => String::from(""),
            };
            let ret = Volume {
                title,
                id: String::from(vol),
            };
            Some(ret)
        })
        .collect::<Vec<_>>();

    Ok(ret)
}

async fn download_np(vol: &str, path: &PathBuf) -> Result<(), Box<dyn Error>> {
    // fetch page
    const URL: &str = "https://post.naver.com/viewer/postView.nhn";
    let body = CLIENT
        .get(URL)
        .query(&[("volumeNo", vol)])
        .send()
        .await?
        .text()
        .await?;

    // real body is hidden inside a <script>, extract it
    let document = Html::parse_document(&body);
    let fragment = extract_real_body(&document);

    // extract useful fields
    let date = extract_date(&document);
    let title = extract_title(&document);
    let imgs = extract_images(&fragment);
    if imgs.is_empty() {
        println!("No images found!");
    }
    // println!("date: {}", date);
    // println!("title: {}", title);
    // println!("images: {}", imgs.len());

    // check if already downloaded
    let full_path = path.join(format!("{}-{}-{}/", date, vol, title));
    if full_path.exists() {
        return Ok(());
    }

    // create base directory if it doesn't exist
    let _ = std::fs::create_dir_all(&path);

    // create progress bar
    let pb = ProgressBar::new(imgs.len() as u64);
    let sty = ProgressStyle::default_bar()
        .template("[{wide_bar}] {pos:>3}/{len:3}")
        .progress_chars("=> ");
    pb.set_style(sty);

    // download all images
    println!("Downloading {}...", title);
    let temp_dir = tempdir()?;
    futures::stream::iter(imgs.into_iter().enumerate().map(|(i, url)| {
        let ext = extract_extension(&url);
        let filename = format!("{}-{}-{}-img{:03}{}", date, vol, title, i + 1, ext);
        download_image(url.to_owned(), temp_dir.path().join(filename), &pb)
    }))
    .buffer_unordered(20)
    .collect::<Vec<_>>()
    .await;

    // move temp directory
    let options = fs_extra::dir::CopyOptions::new();
    let temp_dir_2 = path.join(temp_dir.path().file_name().unwrap());
    fs_extra::dir::copy(&temp_dir, &path, &options)?;
    std::fs::rename(&temp_dir_2, &full_path)?;

    pb.finish_and_clear();

    Ok(())
}

async fn download_image(
    url: String,
    path: PathBuf,
    pb: &ProgressBar,
) -> Result<(), Box<dyn Error>> {
    // println!("{} {}", url, path.as_os_str().to_str().unwrap());
    let body = reqwest::get(&url).await?.bytes().await?;
    let mut buffer = File::create(path)?;
    buffer.write_all(&body)?;
    pb.inc(1);
    Ok(())
}

fn extract_extension(url: &str) -> String {
    let path = std::path::Path::new(&url);
    match path.extension() {
        Some(ext) => format!(".{}", ext.to_str().unwrap().to_lowercase()),
        None => String::from(""),
    }
}

fn extract_real_body(document: &Html) -> Html {
    lazy_static! {
        static ref BODY_SEL: Selector = Selector::parse("script#__clipContent").unwrap();
    }
    let real_body = document
        .select(&BODY_SEL)
        .map(|elem| htmlescape::decode_html(&elem.inner_html().as_str()).unwrap())
        .collect::<Vec<_>>()
        .join("");
    Html::parse_fragment(&real_body)
}

fn extract_date(fragment: &Html) -> String {
    lazy_static! {
        static ref DATE_SEL: Selector =
            Selector::parse(r#"meta[property="og:createdate"]"#).unwrap();
    }
    let date_raw = fragment
        .select(&DATE_SEL)
        .filter_map(|m| m.value().attr("content"))
        .next()
        .unwrap()
        .trim()
        .replace('\n', "");
    format!("{}{}{}", &date_raw[0..4], &date_raw[5..7], &date_raw[8..10])
}

fn extract_title(fragment: &Html) -> String {
    lazy_static! {
        static ref TITLE_SEL: Selector =
            Selector::parse(r#"meta[property="nv:news:title"]"#).unwrap();
    }
    fragment
        .select(&TITLE_SEL)
        .filter_map(|m| m.value().attr("content"))
        .next()
        .unwrap()
        .trim()
        .replace('\n', "")
}

fn extract_images(fragment: &Html) -> Vec<String> {
    lazy_static! {
        static ref IMG_SEL_1: Selector =
            Selector::parse(".se_mediaImage, .se_background_img").unwrap();
        static ref IMG_SEL_2: Selector = Selector::parse(".img_attachedfile").unwrap();
    }

    let find_images = |sel: &Selector| {
        fragment
            .select(sel)
            .filter_map(|e| e.value().attr("data-src"))
            .map(|url| {
                // remove query string for full size images
                let mut temp = reqwest::Url::parse(url).unwrap();
                temp.query_pairs_mut().clear();
                temp.as_str().trim_end_matches('?').to_owned()
            })
            .collect::<Vec<_>>()
    };

    let ret = find_images(&IMG_SEL_1);
    if !ret.is_empty() {
        return ret;
    }

    find_images(&IMG_SEL_2)
}
