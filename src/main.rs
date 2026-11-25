use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImage, RgbImage};
use serde::Deserialize;

const USER_AGENT: &str = concat!("x-stitch/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per socket read or write, so a slow download survives as long as data keeps arriving.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IMAGE_BYTES: u64 = 100 * 1024 * 1024;
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];

#[derive(Parser, Debug)]
#[command(name = "x-stitch", version, about)]
struct Cli {
    /// One or more X/Twitter status URLs
    #[arg(required = true)]
    urls: Vec<String>,

    /// Output directory (defaults to current directory)
    #[arg(short, long, default_value = ".")]
    out: PathBuf,

    /// Custom output filename (only valid with a single URL)
    #[arg(short, long)]
    name: Option<String>,

    /// JPEG output quality (1-100)
    #[arg(short, long, default_value_t = 95, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,

    /// Stitch images vertically instead of horizontally
    #[arg(long)]
    vertical: bool,

    /// Do not embed the source X post URL in JPEG comment metadata
    #[arg(long)]
    no_metadata: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Horizontal,
    Vertical,
}

struct Options<'a> {
    out_dir: &'a Path,
    custom_name: Option<&'a str>,
    quality: u8,
    orientation: Orientation,
    embed_metadata: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct TweetRef {
    user: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct FxResponse {
    tweet: FxTweet,
}

#[derive(Debug, Deserialize)]
struct FxTweet {
    author: FxAuthor,
    media: Option<FxMedia>,
}

#[derive(Debug, Deserialize)]
struct FxAuthor {
    screen_name: String,
}

#[derive(Debug, Deserialize)]
struct FxMedia {
    #[serde(default)]
    photos: Vec<FxPhoto>,
}

#[derive(Debug, Deserialize)]
struct FxPhoto {
    url: String,
}

struct Photo {
    bytes: Vec<u8>,
    image: DynamicImage,
}

struct SavedImage {
    path: PathBuf,
    width: u32,
    height: u32,
    segments: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.name.is_some() && cli.urls.len() > 1 {
        bail!("--name can only be used with a single URL");
    }
    if cli.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        bail!("--name cannot be empty");
    }

    let agent = build_agent(IO_TIMEOUT);

    let orientation = if cli.vertical {
        Orientation::Vertical
    } else {
        Orientation::Horizontal
    };

    let options = Options {
        out_dir: &cli.out,
        custom_name: cli.name.as_deref(),
        quality: cli.quality,
        orientation,
        embed_metadata: !cli.no_metadata,
    };

    let mut failures = 0usize;
    for raw_url in &cli.urls {
        let result = parse_tweet_url(raw_url)
            .and_then(|tweet_ref| process_tweet(&agent, &tweet_ref, &options));
        match result {
            Ok(saved) => print_saved(&saved),
            Err(err) => {
                failures += 1;
                eprintln!("error: {raw_url}: {err:#}");
            }
        }
    }

    if failures > 0 {
        bail!("{failures} of {} URLs failed", cli.urls.len());
    }

    Ok(())
}

fn build_agent(io_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(io_timeout)
        .timeout_write(io_timeout)
        .user_agent(USER_AGENT)
        .build()
}

fn print_saved(saved: &SavedImage) {
    println!(
        "{} ({}x{}, {} segments)",
        saved.path.display(),
        saved.width,
        saved.height,
        saved.segments
    );
}

fn process_tweet(
    agent: &ureq::Agent,
    tweet_ref: &TweetRef,
    options: &Options<'_>,
) -> Result<SavedImage> {
    let api_url = format!(
        "https://api.fxtwitter.com/{}/status/{}",
        tweet_ref.user, tweet_ref.id
    );
    let resp: FxResponse = agent
        .get(&api_url)
        .call()
        .with_context(|| format!("failed to fetch {api_url}"))?
        .into_json()
        .with_context(|| format!("failed to parse metadata from {api_url}"))?;

    let screen_name = safe_screen_name(&resp.tweet.author.screen_name)?;
    let photos = resp.tweet.media.map(|m| m.photos).unwrap_or_default();

    if photos.is_empty() {
        bail!(
            "no photos found in {}/status/{}",
            tweet_ref.user,
            tweet_ref.id
        );
    }

    let photo_urls: Vec<String> = photos
        .into_iter()
        .map(|p| ensure_orig_url(&p.url))
        .collect();
    let downloads = download_images(agent, &photo_urls)?;
    let segments = downloads.len();
    let (jpeg, width, height) = render(downloads, options.orientation, options.quality)?;

    let source_url = format!("https://x.com/{screen_name}/status/{}", tweet_ref.id);
    let comment = options.embed_metadata.then_some(source_url.as_str());

    let filename = resolve_filename(options.custom_name, screen_name, &tweet_ref.id);
    let out_path = options.out_dir.join(filename);
    let bytes = match comment {
        Some(comment) => insert_jpeg_comment(&jpeg, comment)?,
        None => jpeg,
    };
    fs::create_dir_all(options.out_dir).with_context(|| {
        format!(
            "failed to create output directory {}",
            options.out_dir.display()
        )
    })?;
    fs::write(&out_path, bytes)
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    Ok(SavedImage {
        path: out_path,
        width,
        height,
        segments,
    })
}

fn parse_tweet_url(raw: &str) -> Result<TweetRef> {
    let trimmed = raw.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);

    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);

    let segments: Vec<&str> = without_query.split('/').filter(|s| !s.is_empty()).collect();
    let Some(raw_host) = segments.first() else {
        bail!("invalid X status URL: {raw}");
    };

    let host = raw_host.to_ascii_lowercase();
    let valid_host = matches!(
        host.as_str(),
        "x.com"
            | "www.x.com"
            | "mobile.x.com"
            | "twitter.com"
            | "www.twitter.com"
            | "mobile.twitter.com"
            | "fxtwitter.com"
            | "vxtwitter.com"
            | "fixupx.com"
    );
    if !valid_host {
        bail!("invalid X status URL: {raw}");
    }

    let (user, id) = match segments.as_slice() {
        [_, "i", "web", "status" | "statuses", id, ..] => ("i", *id),
        [_, user, "status" | "statuses", id, ..] => (*user, *id),
        _ => bail!("invalid X status URL: {raw}"),
    };

    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid X status URL: {raw}");
    }

    Ok(TweetRef {
        user: user.to_string(),
        id: id.to_string(),
    })
}

fn ensure_orig_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return format!("{url}?name=orig");
    };

    let mut replaced = false;
    let mut params: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|param| {
            let key = param.split_once('=').map_or(param, |(k, _)| k);
            if key == "name" {
                replaced = true;
                "name=orig".to_string()
            } else {
                param.to_string()
            }
        })
        .collect();

    if !replaced {
        params.push("name=orig".to_string());
    }

    format!("{base}?{}", params.join("&"))
}

fn download_images(agent: &ureq::Agent, urls: &[String]) -> Result<Vec<Photo>> {
    thread::scope(|scope| {
        let handles: Vec<_> = urls
            .iter()
            .map(|url| {
                scope.spawn(move || -> Result<Photo> {
                    let resp = agent
                        .get(url)
                        .call()
                        .with_context(|| format!("failed to download {url}"))?;

                    let mut buf = Vec::new();
                    resp.into_reader()
                        .take(MAX_IMAGE_BYTES + 1)
                        .read_to_end(&mut buf)
                        .with_context(|| format!("failed to read body from {url}"))?;

                    if buf.len() as u64 > MAX_IMAGE_BYTES {
                        bail!("image {url} exceeds maximum size of {MAX_IMAGE_BYTES} bytes");
                    }

                    let image = image::load_from_memory(&buf)
                        .with_context(|| format!("failed to decode image from {url}"))?;
                    Ok(Photo { bytes: buf, image })
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|h| h.join().expect("download thread panicked"))
            .collect()
    })
}

/// Returns the output JPEG and its dimensions. A lone JPEG is passed through untouched,
/// since re-encoding it would only lose quality.
fn render(
    mut photos: Vec<Photo>,
    orientation: Orientation,
    quality: u8,
) -> Result<(Vec<u8>, u32, u32)> {
    if photos.len() == 1 && photos[0].bytes.starts_with(&JPEG_SOI) {
        let photo = photos.remove(0);
        return Ok((photo.bytes, photo.image.width(), photo.image.height()));
    }

    let images: Vec<DynamicImage> = photos.into_iter().map(|p| p.image).collect();
    let stitched = stitch_images(&images, orientation)?;
    let jpeg = encode_jpeg(&stitched, quality)?;
    Ok((jpeg, stitched.width(), stitched.height()))
}

fn stitch_images(images: &[DynamicImage], orientation: Orientation) -> Result<RgbImage> {
    if images.is_empty() {
        bail!("cannot stitch empty image list");
    }

    let vertical = orientation == Orientation::Vertical;
    // (length along the stitch axis, length across it)
    let dims = |img: &DynamicImage| {
        if vertical {
            (img.height(), img.width())
        } else {
            (img.width(), img.height())
        }
    };

    let along = images
        .iter()
        .try_fold(0u32, |acc, img| acc.checked_add(dims(img).0))
        .context("stitched image size overflow")?;
    let across = images.iter().map(|img| dims(img).1).max().unwrap_or(0);
    if images.iter().any(|img| dims(img).1 != across) {
        eprintln!(
            "warning: segments differ in {}; smaller ones are padded with black",
            if vertical { "width" } else { "height" }
        );
    }

    let (width, height) = if vertical {
        (across, along)
    } else {
        (along, across)
    };
    let mut canvas = RgbImage::new(width, height);
    let mut offset = 0u32;
    for img in images {
        let (x, y) = if vertical { (0, offset) } else { (offset, 0) };
        canvas
            .copy_from(&img.to_rgb8(), x, y)
            .context("failed to copy segment")?;
        offset += dims(img).0;
    }
    Ok(canvas)
}

/// The screen name comes from the metadata API and ends up in the output path.
fn safe_screen_name(name: &str) -> Result<&str> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("unexpected screen name in API response: {name:?}");
    }
    Ok(name)
}

fn resolve_filename(custom_name: Option<&str>, screen_name: &str, status_id: &str) -> String {
    match custom_name {
        Some(name) => {
            let trimmed = name.trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                trimmed.to_string()
            } else {
                format!("{trimmed}.jpg")
            }
        }
        None => format!("{screen_name}-{status_id}.jpg"),
    }
}

fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode_image(img)
        .context("failed to encode JPEG")?;
    Ok(jpeg)
}

fn insert_jpeg_comment(raw: &[u8], comment: &str) -> Result<Vec<u8>> {
    if !raw.starts_with(&JPEG_SOI) {
        bail!("JPEG missing SOI marker");
    }

    let comment_bytes = comment.as_bytes();
    let seg_len = u16::try_from(comment_bytes.len() + 2).context("JPEG comment too long")?;

    // Insert JPEG COM (0xFF 0xFE) after APP0 (JFIF) if present, otherwise right after SOI.
    let insert_at = if raw.len() >= 6 && raw[2] == 0xFF && raw[3] == 0xE0 {
        let app0_len = u16::from_be_bytes([raw[4], raw[5]]) as usize;
        let app0_end = 4 + app0_len;
        if app0_len >= 2 && app0_end <= raw.len() {
            app0_end
        } else {
            2
        }
    } else {
        2
    };

    let mut out = Vec::with_capacity(raw.len() + comment_bytes.len() + 4);
    out.extend_from_slice(&raw[..insert_at]);
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(comment_bytes);
    out.extend_from_slice(&raw[insert_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgb};
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn parses_valid_urls() {
        assert_eq!(
            parse_tweet_url("https://x.com/user/status/1234567890").unwrap(),
            TweetRef {
                user: "user".into(),
                id: "1234567890".into(),
            }
        );
        assert_eq!(
            parse_tweet_url("https://twitter.com/alice/status/12345/photo/1?s=20").unwrap(),
            TweetRef {
                user: "alice".into(),
                id: "12345".into(),
            }
        );
        assert_eq!(
            parse_tweet_url("x.com/bob/status/999/").unwrap(),
            TweetRef {
                user: "bob".into(),
                id: "999".into(),
            }
        );
        assert_eq!(
            parse_tweet_url("https://x.com/i/web/status/1899999999999999999").unwrap(),
            TweetRef {
                user: "i".into(),
                id: "1899999999999999999".into(),
            }
        );
        assert_eq!(
            parse_tweet_url("https://twitter.com/carol/statuses/42").unwrap(),
            TweetRef {
                user: "carol".into(),
                id: "42".into(),
            }
        );
    }

    #[test]
    fn rejects_invalid_urls() {
        assert!(parse_tweet_url("https://example.com/user/status/123").is_err());
        assert!(parse_tweet_url("https://x.com/user").is_err());
        assert!(parse_tweet_url("https://x.com/user/status/notanumber").is_err());
        assert!(parse_tweet_url("").is_err());
    }

    #[test]
    fn upgrades_image_url_to_orig() {
        assert_eq!(
            ensure_orig_url("https://pbs.twimg.com/media/abc.jpg"),
            "https://pbs.twimg.com/media/abc.jpg?name=orig"
        );
        assert_eq!(
            ensure_orig_url("https://pbs.twimg.com/media/abc?format=jpg"),
            "https://pbs.twimg.com/media/abc?format=jpg&name=orig"
        );
        assert_eq!(
            ensure_orig_url("https://pbs.twimg.com/media/abc?format=jpg&name=large"),
            "https://pbs.twimg.com/media/abc?format=jpg&name=orig"
        );
        assert_eq!(
            ensure_orig_url("https://pbs.twimg.com/media/abc?name=medium&format=png"),
            "https://pbs.twimg.com/media/abc?name=orig&format=png"
        );
    }

    #[test]
    fn resolves_filenames() {
        assert_eq!(resolve_filename(None, "user", "123"), "user-123.jpg");
        assert_eq!(resolve_filename(None, "user_1", "123"), "user_1-123.jpg");
        assert_eq!(
            resolve_filename(Some("panorama"), "user", "123"),
            "panorama.jpg"
        );
        assert_eq!(
            resolve_filename(Some("panorama.jpg"), "user", "123"),
            "panorama.jpg"
        );
        assert_eq!(
            resolve_filename(Some("PANORAMA.JPG"), "user", "123"),
            "PANORAMA.JPG"
        );
    }

    #[test]
    fn rejects_screen_names_that_are_not_plain_handles() {
        for bad in ["../../etc/x", "a/b", "", "..", "a b"] {
            assert!(safe_screen_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(safe_screen_name("user_1").unwrap(), "user_1");
    }

    #[test]
    fn slow_body_is_not_cut_off_while_data_keeps_arriving() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            let _ = stream.read(&mut [0u8; 1024]);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
                .unwrap();
            for byte in b"data" {
                thread::sleep(Duration::from_millis(100));
                stream.write_all(&[*byte]).unwrap();
            }
        });

        // The body takes 400ms in total, with at most 100ms between bytes.
        let agent = build_agent(Duration::from_millis(250));
        let mut body = Vec::new();
        agent
            .get(&format!("http://{addr}/"))
            .call()
            .unwrap()
            .into_reader()
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, b"data");
    }

    #[test]
    fn stitches_horizontally_and_vertically() {
        let red = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 3, Rgb([255, 0, 0])));
        let blue = DynamicImage::ImageRgb8(RgbImage::from_pixel(4, 3, Rgb([0, 0, 255])));

        let h = stitch_images(&[red.clone(), blue.clone()], Orientation::Horizontal).unwrap();
        assert_eq!(h.dimensions(), (6, 3));
        assert_eq!(*h.get_pixel(0, 0), Rgb([255, 0, 0]));
        assert_eq!(*h.get_pixel(2, 0), Rgb([0, 0, 255]));

        let v = stitch_images(&[red, blue], Orientation::Vertical).unwrap();
        assert_eq!(v.dimensions(), (4, 6));
        assert_eq!(*v.get_pixel(0, 0), Rgb([255, 0, 0]));
        assert_eq!(*v.get_pixel(0, 3), Rgb([0, 0, 255]));
    }

    #[test]
    fn passes_a_lone_jpeg_through_and_re_encodes_everything_else() {
        let image = RgbImage::from_pixel(4, 4, Rgb([10, 20, 30]));
        let original = encode_jpeg(&image, 50).unwrap();
        let photo = |bytes: &[u8]| Photo {
            bytes: bytes.to_vec(),
            image: DynamicImage::ImageRgb8(image.clone()),
        };

        let (jpeg, width, height) =
            render(vec![photo(&original)], Orientation::Horizontal, 95).unwrap();
        assert_eq!(jpeg, original);
        assert_eq!((width, height), (4, 4));

        let (jpeg, ..) = render(vec![photo(b"\x89PNG")], Orientation::Horizontal, 95).unwrap();
        assert!(jpeg.starts_with(&JPEG_SOI));

        let (jpeg, width, height) = render(
            vec![photo(&original), photo(&original)],
            Orientation::Horizontal,
            95,
        )
        .unwrap();
        assert_ne!(jpeg, original);
        assert_eq!((width, height), (8, 4));
    }

    #[test]
    fn embeds_jpeg_comment_after_app0_and_skips_when_none() {
        let img = RgbImage::from_pixel(4, 4, Rgb([10, 20, 30]));
        let url = "https://x.com/user/status/1234567890";

        let without_meta = encode_jpeg(&img, 95).unwrap();
        let with_meta = insert_jpeg_comment(&without_meta, url).unwrap();
        assert_eq!(&with_meta[0..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
        let app0_len = u16::from_be_bytes([with_meta[4], with_meta[5]]) as usize;
        let com_start = 4 + app0_len;
        assert_eq!(&with_meta[com_start..com_start + 2], &[0xFF, 0xFE]);
        let seg_len =
            u16::from_be_bytes([with_meta[com_start + 2], with_meta[com_start + 3]]) as usize;
        assert_eq!(
            &with_meta[com_start + 4..com_start + 2 + seg_len],
            url.as_bytes()
        );
        assert_eq!(
            image::load_from_memory(&with_meta).unwrap().dimensions(),
            (4, 4)
        );

        assert_eq!(&without_meta[0..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert_ne!(&without_meta[com_start..com_start + 2], &[0xFF, 0xFE]);
    }
}
