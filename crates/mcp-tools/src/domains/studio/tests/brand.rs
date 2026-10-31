//! Brand images and brand sounds through the studio tool, against a stub of Create: work
//! asked for by name (form, sizes, voice), the plan's sizes and files, a made piece's files
//! as signed links with its alt text, caption and checks, and revise from a made piece.

use super::*;

const SOUND_ID: &str = "77777777-7777-4777-8777-777777777777";
const FILM_ID: &str = "88888888-8888-4888-8888-888888888888";

fn size(id: &str, label: &str, width: u32, height: u32, aspect: &str) -> Value {
    json!({"id": id, "label": label, "group": "", "width": width, "height": height, "aspect": aspect})
}

fn set_sizes() -> Value {
    json!([
        size("instagram-post", "Instagram post", 1080, 1350, "4:5"),
        size("instagram-story", "Instagram story", 1080, 1920, "9:16"),
        size("linkedin-post", "LinkedIn post", 1200, 1200, "1:1"),
    ])
}

/// The planned pieces: a social set and a sonic logo; a resize from the made set alone.
fn brand_pieces(resize: bool) -> Value {
    if resize {
        return json!([
            {"index": 0, "id": "spring-sale-set", "title": "Spring Sale", "kindOfWork": "image.lifestyle",
             "medium": "image", "aspect": "16:9", "form": "social-set", "from": "resize",
             "sizes": [size("x-post", "X post", 1600, 900, "16:9"), size("link-preview", "Link preview", 1200, 630, "40:21")],
             "exports": ["png", "jpg", "webp", "zip"],
             "why": "the same picture, recomposed for each new frame", "asked": true},
        ]);
    }
    json!([
        {"index": 0, "id": "spring-sale-set", "title": "Spring Sale", "kindOfWork": "image.lifestyle",
         "medium": "image", "aspect": "4:5", "form": "social-set", "sizes": set_sizes(),
         "exports": ["png", "jpg", "webp", "zip"],
         "why": "one idea, posted in each network's own size", "asked": true},
        {"index": 1, "id": "sale-sting", "title": "Sale Sting", "kindOfWork": "music.sting",
         "medium": "sound", "seconds": 4, "form": "sonic-logo",
         "exports": ["mp3", "wav", "broadcast", "stems"],
         "why": "the sale video ends on the brand's sound", "asked": false},
    ])
}

fn brand_quote(status: &str, resize: bool) -> Value {
    let ready = status == "ready";
    json!({
        "id": QUOTE,
        "status": status,
        "error": null,
        "request": {"kind": "make", "prompt": "Announce our spring sale"},
        "plan": if ready { json!({
            "understanding": if resize {
                "the spring sale set again, recomposed for X and link previews"
            } else {
                "a social set announcing the spring sale, and a sonic logo to end its video"
            },
            "pieces": brand_pieces(resize),
        }) } else { Value::Null },
        "credits": if ready { json!(if resize { 60 } else { 380 }) } else { Value::Null },
        "estimatedCredits": if ready { json!(if resize { 40 } else { 240 }) } else { Value::Null },
        "read": {"brand": "Kiln & Co.", "context": [
            {"what": "Brand kit: Kiln & Co.", "detail": "celadon, warm white, the kiln mark"},
            {"what": "From your agent: The sale", "detail": "20% off every bowl"},
        ]},
        "retryAfterSec": if ready { Value::Null } else { json!(2) },
        "createdAt": "2026-10-29T18:00:00Z",
        "expiresAt": "2026-10-30T18:00:00Z",
    })
}

fn file(piece: i64, id: &str, label: &str, format: &str, mime: &str, bytes: u64) -> Value {
    json!({
        "id": format!("{piece}-{id}"),
        "name": format!("spring-sale-{id}"),
        "label": label,
        "format": format,
        "mimeType": mime,
        "sizeBytes": bytes,
        "url": format!("/api/v1/create/jobs/{JOB}/files/{piece}-{id}?workspaceId={WORKSPACE}"),
    })
}

fn image_file(
    id: &str,
    size_id: &str,
    label: &str,
    format: &str,
    w: u32,
    h: u32,
    bytes: u64,
) -> Value {
    let mime = match format {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        _ => "image/webp",
    };
    let mut file = file(0, id, label, format, mime, bytes);
    file["width"] = json!(w);
    file["height"] = json!(h);
    file["size"] = json!(size_id);
    file
}

fn sound_file(id: &str, label: &str, format: &str, role: &str, target: f64, bytes: u64) -> Value {
    let mut file = file(
        1,
        id,
        label,
        format,
        if format == "mp3" {
            "audio/mpeg"
        } else {
            "audio/wav"
        },
        bytes,
    );
    file["durationMs"] = json!(4000);
    file["role"] = json!(role);
    file["loudness"] = json!({"target": target, "integrated": target - 0.1, "truePeak": -1.4});
    file
}

/// The made set's and the made sting's files, as the producer names them.
fn made_files() -> (Vec<Value>, Vec<Value>) {
    let mut set = Vec::new();
    for (id, label, w, h, mb) in [
        ("instagram-post", "Instagram post", 1080, 1350, 1.2),
        ("instagram-story", "Instagram story", 1080, 1920, 1.6),
        ("linkedin-post", "LinkedIn post", 1200, 1200, 1.3),
    ] {
        let bytes = (mb * 1024.0 * 1024.0) as u64;
        set.push(image_file(
            &format!("{id}.png"),
            id,
            &format!("{label} · PNG"),
            "png",
            w,
            h,
            bytes,
        ));
        set.push(image_file(
            &format!("{id}.jpg"),
            id,
            &format!("{label} · JPG"),
            "jpg",
            w,
            h,
            bytes / 4,
        ));
        set.push(image_file(
            &format!("{id}.webp"),
            id,
            &format!("{label} · WebP"),
            "webp",
            w,
            h,
            bytes / 5,
        ));
    }
    set.push(file(
        0,
        "set.zip",
        "All 3 sizes · ZIP (PNG, JPG, WebP)",
        "zip",
        "application/zip",
        6_100_000,
    ));
    let sting = vec![
        sound_file(
            "web.mp3",
            "MP3 · −16 LUFS, for the web",
            "mp3",
            "web",
            -16.0,
            98_304,
        ),
        sound_file(
            "web.wav",
            "WAV · −16 LUFS, for the web",
            "wav",
            "web",
            -16.0,
            768_000,
        ),
        sound_file(
            "broadcast.wav",
            "WAV · −23 LUFS, for broadcast (24-bit)",
            "wav",
            "broadcast",
            -23.0,
            1_152_000,
        ),
        sound_file(
            "broadcast.mp3",
            "MP3 · −23 LUFS, for broadcast",
            "mp3",
            "broadcast",
            -23.0,
            98_304,
        ),
        {
            let mut stems = file(
                1,
                "stems.zip",
                "Stems · music, effects (WAV)",
                "zip",
                "application/zip",
                1_400_000,
            );
            stems["role"] = json!("stem");
            stems
        },
    ];
    (set, sting)
}

fn brand_job(status: &str, made: bool) -> Value {
    let done = status == "succeeded";
    let (set_files, sting_files) = made_files();
    let mut plan = brand_pieces(false);
    for piece in plan.as_array_mut().unwrap() {
        let running = if piece["index"] == 0 {
            "judging"
        } else {
            "making"
        };
        piece["status"] = json!(if made { "made" } else { running });
    }
    json!({
        "id": JOB,
        "workspaceId": WORKSPACE,
        "kind": "make",
        "status": status,
        "error": null,
        "progress": if done { 100 } else { 55 },
        "request": {"kind": "make", "prompt": "Announce our spring sale"},
        "outputs": if made { json!([
            {"index": 0, "contentId": IMAGE_ID, "mimeType": "image/png",
             "url": format!("/api/v1/create/outputs/{IMAGE_ID}/content?workspaceId={WORKSPACE}")},
            {"index": 1, "contentId": SOUND_ID, "mimeType": "audio/mpeg", "durationMs": 4000,
             "url": format!("/api/v1/create/outputs/{SOUND_ID}/content?workspaceId={WORKSPACE}")},
        ]) } else { json!([]) },
        "quotedCredits": 380,
        "chargedCredits": if done { json!(236) } else { Value::Null },
        "make": {
            "quoteId": QUOTE,
            "plan": {"understanding": "u", "pieces": plan},
            "read": {"brand": "Kiln & Co.", "context": []},
            "stage": match status {
                "succeeded" => json!("done"),
                "queued" => Value::Null,
                _ => json!("judging spring-sale-set"),
            },
            "spentCredits": if status == "queued" { 0 } else { 150 },
            "pieces": if made { json!([
                {"index": 0, "id": "spring-sale-set", "title": "Spring Sale", "medium": "image", "form": "social-set",
                 "alt": "Three celadon bowls on a sunlit shelf under the words “Spring Sale, 20% off”.",
                 "caption": "Our spring sale starts Saturday: 20% off every bowl, through Monday.",
                 "placard": {"asked": "Announce our spring sale", "idea": "morning light on the shelf the bowls are thrown for", "made": "One picture, recomposed for each network's frame, the kiln mark on every one"},
                 "files": set_files,
                 "sizes": [
                    {"id": "instagram-post", "label": "Instagram post", "width": 1080, "height": 1350, "made": "kept", "mark": true, "file": "0-instagram-post.png"},
                    {"id": "instagram-story", "label": "Instagram story", "width": 1080, "height": 1920, "made": "recomposed", "mark": true, "file": "0-instagram-story.png"},
                    {"id": "linkedin-post", "label": "LinkedIn post", "width": 1200, "height": 1200, "made": "recomposed", "mark": true, "file": "0-linkedin-post.png"},
                 ],
                 "checks": [
                    {"name": "Type inside the safe area", "ok": true, "size": "instagram-post"},
                    {"name": "Type inside the safe area", "ok": false, "detail": "the price sits under the story's reply bar", "size": "instagram-story"},
                    {"name": "Mark contrast", "ok": true},
                    {"name": "Reads on a phone", "ok": true},
                 ]},
                {"index": 1, "id": "sale-sting", "title": "Sale Sting", "medium": "sound", "form": "sonic-logo",
                 "caption": "Two bright notes rising into the brand's warm chord.",
                 "placard": {"asked": "Announce our spring sale", "idea": "a kiln's ping, then the chord", "made": "Three takes, judged; mixed and mastered"},
                 "files": sting_files,
                 "waveform": vec![0.5; 64],
                 "checks": [
                    {"name": "Web master: loudness", "ok": true, "detail": "−16.1 LUFS"},
                    {"name": "Broadcast master: loudness", "ok": true, "detail": "−23.1 LUFS"},
                    {"name": "Broadcast master: true peak", "ok": true, "detail": "−1.4 dBTP"},
                 ]},
            ]) } else { json!([]) },
            "failed": [],
        },
        "createdAt": "2026-10-29T18:01:00Z",
    })
}

/// A making of one product film, made.
fn film_job() -> Value {
    json!({
        "id": JOB, "workspaceId": WORKSPACE, "kind": "make", "status": "succeeded", "error": null,
        "progress": 100,
        "request": {"kind": "make", "prompt": "A launch film", "references": [{"contentId": IMAGE_ID, "role": "logo"}]},
        "outputs": [{"index": 0, "contentId": FILM_ID, "mimeType": "video/mp4", "durationMs": 20000,
                     "url": format!("/api/v1/create/outputs/{FILM_ID}/content?workspaceId={WORKSPACE}")}],
        "quotedCredits": 900, "chargedCredits": 850,
        "make": {"quoteId": QUOTE, "plan": {"understanding": "u", "pieces": [
            {"index": 0, "id": "launch-film", "title": "Launch Film", "kindOfWork": "video.brand", "medium": "video",
             "aspect": "16:9", "seconds": 20, "status": "made"}]},
            "pieces": [{"index": 0, "id": "launch-film", "title": "Launch Film", "medium": "video"}], "failed": []},
        "createdAt": "2026-10-29T18:01:00Z",
    })
}

/// How the stub making stands.
#[derive(Debug, Clone)]
struct Scene {
    job: &'static str,
    /// A file whose link Create can't make.
    unlinked: Option<&'static str>,
}

/// Create for brand work: plans from what was asked, the making as `scene` says, and each
/// file's own signed link.
fn brand_api(scene: Arc<Mutex<Scene>>) -> Responder {
    let asked: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let planning_reads = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &Recorded| {
        let path = request.path.as_str();
        let files_prefix = format!("/create/jobs/{JOB}/files/");
        match (request.method.as_str(), path) {
            ("GET", p) if p.starts_with("/projects/") => {
                Answer::ok(json!({"id": PROJECT, "name": "studio", "workspace_id": WORKSPACE}))
            }
            ("POST", "/create/quotes") => {
                *asked.lock().unwrap() = request.body["request"].clone();
                planning_reads.store(1, Ordering::SeqCst);
                Answer::status(202, brand_quote("planning", false))
            }
            ("GET", p) if p == format!("/create/quotes/{QUOTE}") => {
                let resize = asked.lock().unwrap()["from"]["mode"] == "resize";
                if planning_reads.swap(0, Ordering::SeqCst) == 1 {
                    Answer::ok(brand_quote("planning", resize))
                } else {
                    Answer::ok(brand_quote("ready", resize))
                }
            }
            ("GET", "/credits/balance") => Answer::raw(
                200,
                json!({"credits": {"spendable": 5000, "total_available": 5000}}),
            ),
            ("POST", "/create/jobs") => Answer::status(202, brand_job("queued", false)),
            ("GET", "/create/jobs") => Answer::ok(json!([brand_job("succeeded", true)])),
            ("GET", p) if p == format!("/create/jobs/{JOB}") => {
                let scene = scene.lock().unwrap().clone();
                Answer::ok(match scene.job {
                    "film" => film_job(),
                    "running" => brand_job("generating", false),
                    _ => brand_job("succeeded", true),
                })
            }
            ("GET", p) if p.starts_with(&files_prefix) && p.ends_with("/download-url") => {
                let id = p[files_prefix.len()..]
                    .trim_end_matches("/download-url")
                    .to_string();
                if !request.query.contains(&format!("workspaceId={WORKSPACE}"))
                    || scene.lock().unwrap().unlinked == Some(id.as_str())
                {
                    return Answer::error(404, "NOT_FOUND", "Not found: no such file");
                }
                Answer::ok(json!({
                    "url": format!("https://files.example/{id}?sig=1"),
                    "name": format!("spring-sale-{id}"),
                    "mimeType": "application/octet-stream",
                    "sizeBytes": 1000,
                    "expiresInSeconds": 900,
                }))
            }
            ("GET", p) if p.ends_with("/download-url") => Answer::ok(json!({
                "download_url": format!("https://store.example/{}?sig=1", p.split('/').nth(4).unwrap_or("")),
                "expires_in_seconds": 900
            })),
            ("GET", p) if p == format!("/create/outputs/{IMAGE_ID}/content") => {
                Answer::bytes("image/png", PNG)
            }
            _ => Answer::error(404, "NOT_FOUND", "Not found: nothing here"),
        }
    })
}

async fn brand_stub(job: &'static str) -> (Stub, Arc<Mutex<Scene>>) {
    let scene = Arc::new(Mutex::new(Scene {
        job,
        unlinked: None,
    }));
    (Stub::start(brand_api(scene.clone())).await, scene)
}

fn links(result: &ToolResult) -> Vec<(String, String, Option<String>)> {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            ContentItem::ResourceLink {
                uri,
                name,
                mime_type,
                ..
            } => Some((uri.clone(), name.clone(), mime_type.clone())),
            _ => None,
        })
        .collect()
}

// ============================================================================
// plan
// ============================================================================

#[tokio::test]
async fn plan_asks_for_brand_work_by_name_and_shows_its_sizes_and_files() {
    let (stub, _) = brand_stub("made").await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({
            "action": "plan",
            "prompt": "Announce our spring sale on Instagram and LinkedIn",
            "form": "Social set",
            "sizes": "instagram-post, Instagram story, linkedin-post",
            "wait_seconds": 20,
        }))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let request = &stub.posts_to("/create/quotes")[0].body["request"];
    assert_eq!(
        request,
        &json!({"kind": "make", "prompt": "Announce our spring sale on Instagram and LinkedIn",
                "form": "social-set", "sizes": ["instagram-post", "instagram-story", "linkedin-post"]})
    );
    let text = text_of(&result);
    for line in [
        "1. Spring Sale (Image · Social set · 3 sizes)",
        "   Sizes: Instagram post 1080×1350, Instagram story 1080×1920, LinkedIn post 1200×1200.",
        "   Files: PNG, JPG, WebP and a ZIP of the set.",
        "2. Sale Sting (Sound · Sonic logo · 4 seconds), suggested",
        "   Files: WAV and MP3 at −16 LUFS for the web, a −23 LUFS broadcast master, and stems.",
        "Price: up to 380 credits, likely about 240.",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    let out = structured(&result);
    assert_eq!(
        out["pieces"][0]["delivery"]["sizes"],
        "Instagram post 1080×1350, Instagram story 1080×1920, LinkedIn post 1200×1200"
    );
    assert_eq!(out["pieces"][0]["delivery"]["form_label"], "Social set");
    assert_eq!(
        out["pieces"][1]["delivery"]["files"],
        "WAV and MP3 at −16 LUFS for the web, a −23 LUFS broadcast master, and stems"
    );
    // The plan's own fields stay as Create sent them.
    assert_eq!(out["pieces"][0]["sizes"][1]["height"], 1920);

    // A form that speaks takes a voice.
    let voiced = tool
        .execute(json!({"action": "plan", "prompt": "Read our welcome", "form": "voice-over", "voice": "calm guide", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!voiced.is_error, "{}", text_of(&voiced));
    let request = &stub.posts_to("/create/quotes")[1].body["request"];
    assert_eq!(request["form"], "voice-over");
    assert_eq!(request["voice"], "calm-guide");
    assert!(request.get("sizes").is_none());
}

#[tokio::test]
async fn brand_work_that_does_not_fit_is_refused_before_anything_is_sent() {
    let (stub, _) = brand_stub("made").await;
    let tool = stub.tool();
    let seven: Vec<&str> = forms::SIZES.iter().take(7).map(|s| s.id).collect();
    for (asked, said) in [
        (
            json!({"form": "banner"}),
            "Unknown form “banner”. Use one of: social-set (one post in each network's size)",
        ),
        (
            json!({"form": "sonic-logo", "sizes": ["instagram-post"]}),
            "A sonic logo is a sound: sizes are for an image form",
        ),
        (
            json!({"form": "poster", "sizes": ["instagram-post"]}),
            "A poster is a printed sheet",
        ),
        (
            json!({"form": "social-set", "voice": "gentle"}),
            "A social set has no voice: voice is for voice-over, podcast-intro, podcast-outro",
        ),
        (
            json!({"form": "ad-set", "sizes": seven}),
            "Up to 6 sizes for one image; 7 were asked.",
        ),
        (
            json!({"sizes": ["instagram-reel"]}),
            "Unknown size “instagram-reel”. Sizes: instagram-post (1080×1350)",
        ),
        (
            json!({"voice": "x".repeat(101)}),
            "voice: a voice's name or id, up to 100 characters (voices lists them).",
        ),
    ] {
        let mut input = json!({"action": "plan", "prompt": "Something for the sale"});
        for (key, value) in asked.as_object().unwrap() {
            input[key] = value.clone();
        }
        let error = tool.execute(input).await.unwrap_err();
        assert!(matches!(error, Error::Validation(_)), "{error}");
        assert!(error.to_string().contains(said), "{said}\n---\n{error}");
    }
    assert!(stub.posts_to("/create/quotes").is_empty());
}

// ============================================================================
// result
// ============================================================================

#[tokio::test]
async fn result_links_each_file_with_its_alt_text_caption_and_checks() {
    let (stub, _) = brand_stub("made").await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "result", "make_id": JOB}))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    for line in [
        "1. Spring Sale (Image · Social set · 3 sizes)",
        "   Shown above.",
        "   Alt text: Three celadon bowls on a sunlit shelf under the words “Spring Sale, 20% off”.",
        "   Caption: Our spring sale starts Saturday: 20% off every bowl, through Monday.",
        "   Sizes: Instagram post 1080×1350, Instagram story 1080×1920, LinkedIn post 1200×1200.",
        "   Files (links valid about 15 minutes):",
        "   - Instagram post · PNG · 1080×1350 · 1.2 MB: https://files.example/0-instagram-post.png?sig=1",
        "   - All 3 sizes · ZIP (PNG, JPG, WebP) · 5.8 MB: https://files.example/0-set.zip?sig=1",
        "   - LinkedIn post · WebP · 1200×1200 · 266 KB: https://files.example/0-linkedin-post.webp?sig=1",
        "   Checks: 3 of 4 passed. Not passed: Type inside the safe area (Instagram story): the price sits under the story's reply bar.",
        "2. Sale Sting (Sound · Sonic logo · 4 seconds)",
        "   Caption: Two bright notes rising into the brand's warm chord.",
        "   - MP3 · −16 LUFS, for the web · 96 KB: https://files.example/1-web.mp3?sig=1",
        "   - WAV · −23 LUFS, for broadcast (24-bit) · 1.1 MB: https://files.example/1-broadcast.wav?sig=1",
        "   - Stems · music, effects (WAV) · 1.3 MB: https://files.example/1-stems.zip?sig=1",
        "   Checks: all 3 passed.",
        "   Idea: Morning light on the shelf the bowls are thrown for.",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    // Each size's PNG first, then the ZIP; the web MP3, the broadcast WAV and the stems first.
    let set_at = text.find("0-instagram-post.png").unwrap();
    assert!(set_at < text.find("0-set.zip").unwrap());
    assert!(text.find("0-set.zip").unwrap() < text.find("0-instagram-post.jpg").unwrap());
    assert!(text.find("1-web.mp3").unwrap() < text.find("1-broadcast.wav").unwrap());
    // The files replace the one link a piece without them has.
    assert!(!text.contains("   Link (valid about"), "{text}");

    // The main image inline as before; the main downloads as resource links.
    assert!(result
        .content
        .iter()
        .any(|item| matches!(item, ContentItem::Image { .. })));
    let linked: Vec<String> = links(&result).into_iter().map(|(uri, ..)| uri).collect();
    assert_eq!(
        linked,
        [
            "https://files.example/0-set.zip?sig=1",
            "https://files.example/1-web.mp3?sig=1",
            "https://files.example/1-broadcast.wav?sig=1",
            "https://files.example/1-stems.zip?sig=1",
        ]
    );
    assert_eq!(links(&result)[1].1, "MP3 · −16 LUFS, for the web");
    assert_eq!(links(&result)[1].2.as_deref(), Some("audio/mpeg"));

    let out = structured(&result);
    let set = &out["pieces"][0];
    assert_eq!(set["form"], "social-set");
    assert_eq!(
        set["alt"],
        "Three celadon bowls on a sunlit shelf under the words “Spring Sale, 20% off”."
    );
    assert_eq!(
        set["caption"],
        "Our spring sale starts Saturday: 20% off every bowl, through Monday."
    );
    assert_eq!(set["files"].as_array().unwrap().len(), 10);
    let first = &set["files"][0];
    assert_eq!(first["id"], "0-instagram-post.png");
    assert_eq!(first["label"], "Instagram post · PNG");
    assert_eq!(first["format"], "png");
    assert_eq!(
        first["url"],
        "https://files.example/0-instagram-post.png?sig=1"
    );
    assert_eq!(first["expires_in_seconds"], 900);
    assert_eq!(first["size"], "instagram-post");
    assert_eq!(
        (first["width"].clone(), first["height"].clone()),
        (json!(1080), json!(1350))
    );
    assert_eq!(set["sizes"][1]["made"], "recomposed");
    assert_eq!(set["checks"].as_array().unwrap().len(), 4);
    assert_eq!(
        set["checks_failed"],
        json!(["Type inside the safe area (Instagram story): the price sits under the story's reply bar"])
    );
    assert!(set.get("more_files").is_none());
    let sting = &out["pieces"][1];
    assert_eq!(sting["files"][1]["loudness"]["target"], -23.0);
    assert_eq!(sting["files"][1]["role"], "broadcast");

    // Every link was asked of the piece's own making, in its workspace.
    let asked: Vec<Recorded> = stub
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with(&format!("/create/jobs/{JOB}/files/")))
        .collect();
    assert_eq!(asked.len(), 15);
    assert!(asked.iter().all(
        |r| r.query == format!("workspaceId={WORKSPACE}") && r.path.ends_with("/download-url")
    ));
}

#[tokio::test]
async fn a_file_without_a_link_is_still_listed_and_a_piece_without_files_reads_as_before() {
    let (stub, scene) = brand_stub("made").await;
    scene.lock().unwrap().unlinked = Some("0-linkedin-post.webp");
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "result", "make_id": JOB, "piece": 0}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(
        text.contains("   - LinkedIn post · WebP · 1200×1200 · 266 KB: no link could be made just now; it is in the ContextStudio library"),
        "{text}"
    );
    assert!(
        text.contains("https://files.example/0-set.zip?sig=1"),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["pieces"][0]["files"][9]["url"], Value::Null);

    // A making from before brand work: its pieces carry no files, and read as they did.
    let plain = Stub::start(api(Arc::new(Mutex::new(("succeeded".into(), true))))).await;
    let result = plain
        .tool()
        .execute(json!({"action": "result", "make_id": JOB}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(
        text.contains("   Link (valid about 15 minutes): https://store.example/"),
        "{text}"
    );
    for absent in ["Files (links", "Alt text:", "Caption:", "Checks:"] {
        assert!(!text.contains(absent), "{absent}\n---\n{text}");
    }
    assert!(structured(&result)["pieces"][0].get("files").is_none());
}

// ============================================================================
// revise
// ============================================================================

#[tokio::test]
async fn revise_starts_from_a_made_image_or_sound_in_each_mode() {
    let (stub, _) = brand_stub("made").await;
    let tool = stub.tool();
    let from = |mode: &str, piece: i64| json!({"jobId": JOB, "piece": piece, "mode": mode});

    // More like it: no change needed.
    let more = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": 0, "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!more.is_error, "{}", text_of(&more));
    assert!(text_of(&more).contains("A plan for more like “Spring Sale”"));
    // The sound, changed as asked: a change makes it a revision.
    let changed = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": "sale-sting", "change": "softer, and end on the low note", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!changed.is_error, "{}", text_of(&changed));
    // A resize, named or implied by sizes alone.
    let resized = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": 0, "mode": "resize", "sizes": ["x-post", "link-preview"], "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!resized.is_error, "{}", text_of(&resized));
    let implied = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": 0, "sizes": "pinterest-pin", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!implied.is_error, "{}", text_of(&implied));

    let posted: Vec<Value> = stub
        .posts_to("/create/quotes")
        .into_iter()
        .map(|r| r.body["request"].clone())
        .collect();
    assert_eq!(
        posted,
        [
            json!({"kind": "make", "prompt": "More like this", "from": from("variation", 0)}),
            json!({"kind": "make", "prompt": "softer, and end on the low note", "from": from("revision", 1)}),
            json!({"kind": "make", "prompt": "Resize for X post and Link preview", "from": from("resize", 0),
                   "sizes": ["x-post", "link-preview"]}),
            json!({"kind": "make", "prompt": "Resize for Pinterest pin", "from": from("resize", 0),
                   "sizes": ["pinterest-pin"]}),
        ]
    );
    // The resize's plan reads as one: its new sizes, and where it started.
    let text = text_of(&resized);
    assert!(
        text.contains("   Sizes: X post 1600×900, Link preview 1200×630."),
        "{text}"
    );
    assert!(
        text.contains("   From a made piece: the same picture in new sizes."),
        "{text}"
    );
    assert!(
        text.contains("A plan for “Spring Sale” in X post and Link preview; nothing is made until it is made like any plan."),
        "{text}"
    );
    assert!(stub.posts_to("/create/jobs").is_empty());

    // Said plainly, and nothing planned.
    for (input, said) in [
        (
            json!({"piece": 1, "mode": "resize", "sizes": ["x-post"]}),
            "Only an image can be resized; “Sale Sting” is a sound.",
        ),
        (
            json!({"piece": 0, "mode": "resize"}),
            "A resize needs sizes",
        ),
        (
            json!({"piece": 0, "mode": "revision"}),
            "A revision needs change",
        ),
        (
            json!({"piece": 0, "voice": "gentle"}),
            "“Spring Sale” is an image: voice is for a sound that speaks.",
        ),
        (json!({"piece": 0, "mode": "crop"}), "Unknown mode “crop”"),
    ] {
        let mut asked = json!({"action": "revise", "make_id": JOB, "wait_seconds": 20});
        for (key, value) in input.as_object().unwrap() {
            asked[key] = value.clone();
        }
        let error = tool.execute(asked).await.unwrap_err();
        assert!(error.to_string().contains(said), "{said}\n---\n{error}");
    }
    assert_eq!(stub.posts_to("/create/quotes").len(), 4);
}

#[tokio::test]
async fn revise_plans_another_like_a_made_film_from_the_earlier_ask() {
    let (stub, _) = brand_stub("film").await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "revise", "make_id": JOB, "change": "open on the street, not the studio", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let request = &stub.posts_to("/create/quotes")[0].body["request"];
    assert_eq!(
        request["prompt"],
        "A launch film\n\nNow make one more piece: another “Launch Film” (video · wide 16:9 · 20 seconds), like the one made before. This time: open on the street, not the studio."
    );
    assert!(request.get("from").is_none());
    assert_eq!(
        request["references"],
        json!([{"contentId": IMAGE_ID, "role": "logo", "optional": true}])
    );
    let error = tool
        .execute(json!({"action": "revise", "make_id": JOB, "mode": "resize", "sizes": ["x-post"]}))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Only an image can be resized; “Launch Film” is a video."),
        "{error}"
    );
}

// ============================================================================
// End to end, as the agent reads it
// ============================================================================

/// One call and what the agent is shown.
fn shown(call: &Value, result: &ToolResult, with_structured: bool) -> String {
    let mut out = format!("> studio {call}\n");
    if result.is_error {
        out.push_str("[is_error]\n");
    }
    out.push_str(&text_of(result));
    out.push('\n');
    for item in &result.content {
        match item {
            ContentItem::Image { data, mime_type } => out.push_str(&format!(
                "[image {mime_type}, {} base64 characters]\n",
                data.len()
            )),
            ContentItem::ResourceLink {
                uri,
                name,
                mime_type,
                ..
            } => out.push_str(&format!(
                "[resource_link “{name}” {} {uri}]\n",
                mime_type.as_deref().unwrap_or("")
            )),
            _ => {}
        }
    }
    if with_structured {
        out.push_str("structuredContent:\n");
        out.push_str(&serde_json::to_string_pretty(&structured(result)).unwrap_or_default());
        out.push('\n');
    }
    out.push('\n');
    out
}

#[tokio::test]
async fn brand_work_end_to_end_as_the_agent_reads_it() {
    let (stub, scene) = brand_stub("running").await;
    let tool = stub.tool();
    let mut transcript = String::new();
    let call = |input: Value| {
        let tool = &tool;
        async move {
            let result = tool.execute(input.clone()).await.unwrap();
            (input, result)
        }
    };

    let (input, plan) = call(json!({
        "action": "plan",
        "prompt": "Announce our spring sale on Instagram and LinkedIn, and a short sonic logo to end the sale video",
        "context": [{"title": "The sale", "text": "20% off every bowl, Saturday to Monday."}],
        "wait_seconds": 20,
    }))
    .await;
    assert!(!plan.is_error, "{}", text_of(&plan));
    transcript.push_str(&shown(&input, &plan, false));

    let (input, made) =
        call(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 380})).await;
    assert!(!made.is_error, "{}", text_of(&made));
    transcript.push_str(&shown(&input, &made, false));

    let (input, running) =
        call(json!({"action": "status", "make_id": JOB, "wait_seconds": 0})).await;
    transcript.push_str(&shown(&input, &running, false));
    scene.lock().unwrap().job = "made";
    let (input, done) = call(json!({"action": "status", "make_id": JOB})).await;
    assert_eq!(structured(&done)["follow_up"]["action"], "result");
    transcript.push_str(&shown(&input, &done, false));

    let (input, result) = call(json!({"action": "result", "make_id": JOB})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    transcript.push_str(&shown(&input, &result, true));

    let (input, revised) = call(json!({
        "action": "revise", "make_id": JOB, "piece": 0, "mode": "resize",
        "sizes": ["x-post", "link-preview"], "wait_seconds": 20,
    }))
    .await;
    assert!(!revised.is_error, "{}", text_of(&revised));
    transcript.push_str(&shown(&input, &revised, false));

    assert!(transcript.contains("Alt text: "));
    assert!(transcript.contains("From a made piece: the same picture in new sizes."));
    println!("=== STUDIO TRANSCRIPT BEGIN ===\n{transcript}=== STUDIO TRANSCRIPT END ===");
    if let Ok(path) = std::env::var("STUDIO_TRANSCRIPT") {
        std::fs::write(path, &transcript).unwrap();
    }
}
