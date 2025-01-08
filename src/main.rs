use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use futures::stream::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use reqwest::header::{self, HeaderValue};
use reqwest::Client;
use scraper::element_ref::ElementRef;
use scraper::{Html, Selector};
use serde::Deserialize;
use tempfile::tempdir;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

lazy_static! {
    static ref ID_RE: Regex = Regex::new(r"volumeNo=(?P<vol>\d+)").unwrap();
}

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
    #[arg(short, long, default_value = "posts")]
    directory: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    Member {
        #[arg(default_value = "29156514")]
        id: String,
        #[arg(short, long)]
        filter: Option<String>,
        #[arg(short, long)]
        limit: Option<usize>,
    },
    Url {
        urls: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client: reqwest::Client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:134.0) Gecko/20100101 Firefox/134.0")
        .build()
        .unwrap();

    match args.command {
        Command::Url { urls } => {
            for url in urls {
                process_one(&client, &url, &args.directory).await?;
            }
        }
        Command::Member { id, filter, limit } => {
            let filter = match filter {
                Some(f) => RegexBuilder::new(&f),
                None => RegexBuilder::new(r".*"),
            }
            .case_insensitive(true)
            .build()?;

            process_member(&client, &id, &args.directory, &filter, limit).await?;
        }
    }

    Ok(())
}

#[derive(Debug)]
enum DownloadNPError {
    ParseError(String),
    FileNameError(PathBuf),
}

impl Error for DownloadNPError {}

impl fmt::Display for DownloadNPError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DownloadNPError::ParseError(s) => write!(f, "Error parsing string: {:?}", s),
            DownloadNPError::FileNameError(p) => write!(f, "Error parsing file name: {:?}", p),
        }
    }
}

#[derive(Debug)]
struct Volume {
    title: Option<String>,
    date: Option<String>,
    id: String,
}

impl PartialEq for Volume {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

async fn process_one(client: &Client, url: &str, path: &Path) -> Result<()> {
    let id = ID_RE
        .captures_iter(url)
        .find_map(|c| c.name("vol"))
        .ok_or_else(|| DownloadNPError::ParseError(url.to_owned()))?
        .as_str()
        .to_owned();
    let vol = Volume {
        id,
        title: None,
        date: None,
    };

    download_np(client, &vol, path).await?;
    Ok(())
}

async fn process_member(
    client: &Client,
    member: &str,
    path: &Path,
    filter: &Regex,
    limit: Option<usize>,
) -> Result<()> {
    let mut page: usize = 1;
    let mut first = true;
    let mut num_found = 0;
    let mut np_vols: Vec<Volume> = vec![];

    let pb = ProgressBar::new_spinner();
    let sty = ProgressStyle::default_bar()
        .template("Retrieving member page {pos:} {spinner}")
        .progress_chars("=> ");
    pb.set_style(sty);

    while first || num_found > 0 {
        pb.set_position(page as u64);
        first = false;
        let page_np_vols = volume_from_member(client, member, page).await?;
        num_found = page_np_vols.len();
        np_vols.extend(page_np_vols);
        np_vols.dedup();
        page += 1;
        if let Some(l) = limit {
            if np_vols.len() >= l {
                np_vols.truncate(l);
                break;
            }
        }
    }
    pb.finish_and_clear();
    np_vols.retain(|vol| match &vol.title {
        Some(title) => filter.is_match(title),
        None => false,
    });

    for vol in np_vols {
        download_np(client, &vol, path).await?;
    }

    Ok(())
}

async fn volume_from_member(client: &Client, member: &str, page: usize) -> Result<Vec<Volume>> {
    lazy_static! {
        static ref SEL: Selector = Selector::parse("li").unwrap();
        static ref TITLE_SEL: Selector = Selector::parse(".tit_feed").unwrap();
        static ref DATE_SEL: Selector = Selector::parse(".date_post").unwrap();
        static ref ESCAPE_RE: Regex = Regex::new(r#"\\(?P<c>[^"n])"#).unwrap();
    }
    const URL: &str = "https://post.naver.com/async/my.nhn";

    #[allow(clippy::upper_case_acronyms)]
    #[derive(Deserialize)]
    struct HTML {
        html: String,
    }

    let text = client
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
            let id = e.value().attr("volumeno")?;

            // get title
            let title = Html::parse_fragment(&e.inner_html())
                .select(&TITLE_SEL)
                .next()
                .map(|v| {
                    v.text()
                        .collect::<Vec<_>>()
                        .join("")
                        .replace('\n', "")
                        .trim()
                        .to_owned()
                });

            // get date
            let date = Html::parse_fragment(&e.inner_html())
                .select(&DATE_SEL)
                .next()
                .map(|v| {
                    v.text()
                        .collect::<Vec<_>>()
                        .join("")
                        .replace('.', "")
                        .trim()
                        .to_owned()
                });

            let ret = Volume {
                title,
                date,
                id: String::from(id),
            };
            Some(ret)
        })
        .collect::<Vec<_>>();

    Ok(ret)
}

async fn download_np(client: &Client, vol: &Volume, path: &Path) -> Result<()> {
    // check if already downloaded
    if vol.title.is_some() && vol.date.is_some() {
        let date = vol.date.as_ref().unwrap();
        let title = vol.title.as_ref().unwrap();

        if date.chars().all(|c: char| c.is_ascii_digit()) {
            let full_path = path.join(format!("{}-{}-{}/", date, vol.id, title));
            if full_path.exists() {
                return Ok(());
            }
        }
    }

    // fetch page
    const URL: &str = "https://post.naver.com/viewer/postView.nhn";
    let body = client
        .get(URL)
        .query(&[("volumeNo", &vol.id)])
        .send()
        .await?
        .text()
        .await?;

    // real body is hidden inside a <script>, extract it
    let document = Html::parse_document(&body);
    let fragment = extract_real_body(&document)?;
    let root = fragment.root_element();

    // extract metadata
    let date = extract_date(&document.root_element())?;
    let title = extract_title(&document.root_element())?;

    // check if already downloaded
    let full_path = path.join(format!("{}-{}-{}/", date, vol.id, title));
    if full_path.exists() {
        return Ok(());
    }

    // extract images
    let imgs = extract_images(&root)?;
    if imgs.is_empty() {
        println!("No images found for vol: {}", vol.id);
        return Ok(());
    }

    // create base directory if it doesn't exist
    let _ = std::fs::create_dir_all(path);

    // create progress bar
    let pb = ProgressBar::new(imgs.len() as u64);
    let sty = ProgressStyle::default_bar()
        .template("[{wide_bar}] {pos:>3}/{len:3}")
        .progress_chars("=> ");
    pb.set_style(sty);

    // download all images
    println!("{}...", title);
    let temp_dir = tempdir()?;
    futures::stream::iter(imgs.into_iter().enumerate().map(|(i, url)| {
        let ext = extract_extension(&url);
        let filename = format!("{}-{}-{}-img{:03}{}", date, vol.id, title, i + 1, ext);
        download_image(client, url, temp_dir.path().join(filename), &pb)
    }))
    .buffer_unordered(20)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<_>>()?;

    // move temp directory
    let options = fs_extra::dir::CopyOptions::new();
    let temp_dir_2 = path.join(
        temp_dir
            .path()
            .file_name()
            .ok_or_else(|| DownloadNPError::FileNameError(temp_dir.path().to_path_buf()))?,
    );
    fs_extra::dir::copy(&temp_dir, path, &options)?;
    std::fs::rename(&temp_dir_2, &full_path)?;

    pb.finish_and_clear();

    Ok(())
}

async fn download_image(
    client: &Client,
    url: String,
    path: PathBuf,
    pb: &ProgressBar,
) -> Result<()> {
    let body = client
        .get(&url)
        .header(
            header::REFERER,
            HeaderValue::from_static("https://m.post.naver.com/"),
        )
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let mut buffer = File::create(path)?;
    buffer.write_all(&body)?;
    pb.inc(1);
    Ok(())
}

fn extract_extension(url: &str) -> String {
    let path = std::path::Path::new(&url);
    match path.extension() {
        Some(ext) => format!(".{}", ext.to_string_lossy().to_lowercase()),
        None => String::from(""),
    }
}

fn extract_real_body(document: &Html) -> Result<Html> {
    lazy_static! {
        static ref BODY_SEL: Selector = Selector::parse("script#__clipContent").unwrap();
    }
    let real_body = document
        .select(&BODY_SEL)
        .filter_map(|elem| htmlescape::decode_html(elem.inner_html().as_str()).ok())
        .collect::<Vec<_>>()
        .join("");
    Ok(Html::parse_fragment(&real_body))
}

fn extract_date(element: &ElementRef) -> Result<String> {
    lazy_static! {
        static ref DATE_SEL: Selector =
            Selector::parse(r#"meta[property="og:createdate"]"#).unwrap();
    }
    let date_raw = element
        .select(&DATE_SEL)
        .find_map(|m| m.value().attr("content"))
        .ok_or_else(|| DownloadNPError::ParseError(element.html()))?
        .trim()
        .replace('\n', "");
    Ok(format!(
        "{}{}{}",
        &date_raw[0..4],
        &date_raw[5..7],
        &date_raw[8..10]
    ))
}

fn extract_title(element: &ElementRef) -> Result<String> {
    lazy_static! {
        static ref TITLE_SEL: Selector =
            Selector::parse(r#"meta[property="nv:news:title"]"#).unwrap();
    }
    let ret = element
        .select(&TITLE_SEL)
        .find_map(|m| m.value().attr("content"))
        .ok_or_else(|| DownloadNPError::ParseError(element.html()))?
        .trim()
        .replace('\n', "");
    Ok(ret)
}

fn extract_images(element: &ElementRef) -> Result<Vec<String>> {
    lazy_static! {
        static ref IMG_SEL_1: Selector = Selector::parse("img.se_mediaImage").unwrap();
        static ref IMG_SEL_2: Selector = Selector::parse("img.img_attachedfile").unwrap();
    }

    let find_images = |sel: &Selector| {
        element
            .select(sel)
            .filter_map(|e| {
                let url = e.value().attr("data-src")?;
                if !url.contains("post-phinf.pstatic.net") {
                    return None;
                }
                let mut temp = reqwest::Url::parse(url).ok()?;
                temp.query_pairs_mut().clear();
                Some(temp.as_str().trim_end_matches('?').to_owned())
            })
            .collect::<Vec<_>>()
    };

    let ret = find_images(&IMG_SEL_1);
    if !ret.is_empty() {
        return Ok(ret);
    }

    let ret = find_images(&IMG_SEL_2);
    Ok(ret)
}
