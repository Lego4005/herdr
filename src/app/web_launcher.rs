use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn launch_web_viewer(file_path: &Path) -> io::Result<()> {
    let content = fs::read_to_string(file_path)?;
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown File".to_string());

    let path_str = file_path.to_string_lossy().into_owned();
    let size = content.len();
    let line_count = content.lines().count();

    // Format size nicely
    let size_formatted = if size < 1024 {
        format!("{} B", size)
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size as f64 / 1024.0 / 1024.0)
    };

    let html = generate_html(&filename, &path_str, &size_formatted, line_count, &content);

    let temp_dir = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let temp_file_path = temp_dir.join(format!("herdr-viewer-{}-{}.html", timestamp, pid));

    fs::write(&temp_file_path, html)?;

    #[cfg(target_os = "macos")]
    let open_cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let open_cmd = "xdg-open";

    Command::new(open_cmd).arg(&temp_file_path).spawn()?;

    Ok(())
}

fn generate_html(
    filename: &str,
    filepath: &str,
    size_str: &str,
    line_count: usize,
    raw_content: &str,
) -> String {
    // Escape raw_content for injection in JS script block
    let escaped_content = raw_content
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace("</script>", "<\\/script>");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Herdr Viewer - {filename}</title>
    <!-- Google Fonts -->
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap" rel="stylesheet">
    <!-- Tailwind CSS -->
    <script src="https://cdn.tailwindcss.com"></script>
    <script>
        tailwind.config = {{
            theme: {{
                extend: {{
                    fontFamily: {{
                        sans: ['Outfit', 'sans-serif'],
                        mono: ['JetBrains Mono', 'monospace'],
                    }}
                }}
            }}
        }}
    </script>
    <!-- FontAwesome Icons -->
    <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/font-awesome/6.4.0/css/all.min.css">
    <!-- Marked.js -->
    <script src="https://cdn.jsdelivr.net/npm/marked/marked.min.js"></script>
    <!-- Mermaid.js -->
    <script src="https://cdn.jsdelivr.net/npm/mermaid/dist/mermaid.min.js"></script>
    <!-- Highlight.js -->
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/highlight.js@11.9.0/styles/github-dark.css">
    <script src="https://cdn.jsdelivr.net/npm/highlight.js@11.9.0/lib/highlight.min.js"></script>
    <style>
        body {{
            background-color: #0b0f19;
            color: #e2e8f0;
        }}
        .glass {{
            background: rgba(17, 24, 39, 0.7);
            backdrop-filter: blur(12px);
            border: 1px solid rgba(255, 255, 255, 0.08);
        }}
        .glass-hover:hover {{
            background: rgba(31, 41, 55, 0.8);
            border-color: rgba(255, 255, 255, 0.15);
            transform: translateY(-2px);
            transition: all 0.2s cubic-bezier(0.4, 0, 0.2, 1);
        }}
        .prose-custom table {{
            width: 100%;
            border-collapse: collapse;
            margin: 1.5rem 0;
            background: rgba(17, 24, 39, 0.4);
            border-radius: 8px;
            overflow: hidden;
            border: 1px solid rgba(255, 255, 255, 0.1);
        }}
        .prose-custom th {{
            background: rgba(31, 41, 55, 0.8);
            color: #38bdf8;
            font-weight: 600;
            text-align: left;
            padding: 0.75rem 1rem;
            border-bottom: 2px solid rgba(255, 255, 255, 0.1);
        }}
        .prose-custom td {{
            padding: 0.75rem 1rem;
            border-bottom: 1px solid rgba(255, 255, 255, 0.05);
        }}
        .prose-custom tr:hover {{
            background: rgba(255, 255, 255, 0.02);
        }}
        .prose-custom h1 {{ font-size: 2.25rem; font-weight: 700; margin-top: 2rem; margin-bottom: 1rem; color: #f8fafc; border-bottom: 1px solid rgba(255,255,255,0.1); padding-bottom: 0.5rem; }}
        .prose-custom h2 {{ font-size: 1.5rem; font-weight: 600; margin-top: 1.75rem; margin-bottom: 0.75rem; color: #f1f5f9; }}
        .prose-custom h3 {{ font-size: 1.25rem; font-weight: 600; margin-top: 1.5rem; margin-bottom: 0.5rem; color: #e2e8f0; }}
        .prose-custom p {{ margin-bottom: 1rem; line-height: 1.7; color: #cbd5e1; }}
        .prose-custom ul {{ list-style-type: disc; padding-left: 1.5rem; margin-bottom: 1rem; }}
        .prose-custom li {{ margin-bottom: 0.25rem; color: #cbd5e1; }}
        .prose-custom pre {{ background: #030712; padding: 1rem; border-radius: 8px; overflow-x: auto; border: 1px solid rgba(255,255,255,0.05); margin-bottom: 1.5rem; }}
        .prose-custom code {{ font-family: 'JetBrains Mono', monospace; font-size: 0.875rem; }}
        .prose-custom a {{ color: #38bdf8; text-decoration: none; border-bottom: 1px dashed rgba(56, 189, 248, 0.4); }}
        .prose-custom a:hover {{ border-bottom-style: solid; }}
        .prose-custom blockquote {{ border-left: 4px solid #38bdf8; padding-left: 1rem; margin: 1rem 0; color: #94a3b8; font-style: italic; }}
    </style>
</head>
<body class="min-h-screen flex flex-col font-sans bg-[#080b12] text-slate-100">
    
    <!-- Top Header -->
    <header class="glass sticky top-0 z-50 px-6 py-4 flex items-center justify-between shadow-lg shadow-black/20">
        <div class="flex items-center space-x-3">
            <div class="w-10 h-10 bg-gradient-to-tr from-cyan-500 to-blue-600 rounded-xl flex items-center justify-center shadow-md shadow-cyan-500/20">
                <i class="fa-solid fa-compass-drafting text-white text-lg"></i>
            </div>
            <div>
                <h1 class="text-xl font-bold tracking-tight bg-gradient-to-r from-white to-slate-400 bg-clip-text text-transparent">Herdr Artifact Viewer</h1>
                <p class="text-xs text-cyan-400/80 font-mono select-all">{filepath}</p>
            </div>
        </div>
        <div class="flex items-center space-x-2">
            <button id="btn-render" class="px-4 py-2 rounded-lg font-medium text-sm transition-all duration-200 bg-cyan-600 hover:bg-cyan-500 text-white shadow-lg shadow-cyan-600/10">
                <i class="fa-solid fa-eye mr-2"></i>Rendered View
            </button>
            <button id="btn-raw" class="px-4 py-2 rounded-lg font-medium text-sm transition-all duration-200 bg-slate-800 hover:bg-slate-700 text-slate-300">
                <i class="fa-solid fa-code mr-2"></i>Raw Content
            </button>
        </div>
    </header>

    <!-- Main Container -->
    <div class="flex-1 flex flex-col lg:flex-row overflow-hidden">
        
        <!-- Left Sidebar: Metadata & Metrics Dashboard -->
        <aside class="w-full lg:w-80 p-6 flex flex-col space-y-6 border-r border-slate-800 bg-[#0a0d16]/80 lg:overflow-y-auto">
            
            <!-- File Info Card -->
            <div class="glass p-5 rounded-2xl space-y-4">
                <h2 class="text-xs font-bold text-slate-400 uppercase tracking-widest flex items-center">
                    <i class="fa-solid fa-file-invoice mr-2 text-cyan-500"></i>File Summary
                </h2>
                <div class="space-y-3">
                    <div>
                        <span class="text-xs text-slate-500 block">Name</span>
                        <span class="text-sm font-semibold text-slate-200">{filename}</span>
                    </div>
                    <div>
                        <span class="text-xs text-slate-500 block">Size</span>
                        <span class="text-sm font-semibold text-slate-200">{size_str}</span>
                    </div>
                    <div>
                        <span class="text-xs text-slate-500 block">Line Count</span>
                        <span class="text-sm font-mono font-semibold text-slate-200">{line_count} lines</span>
                    </div>
                </div>
            </div>

            <!-- Dynamically Extracted Metrics (PrismFox-style) -->
            <div class="glass p-5 rounded-2xl space-y-4">
                <h2 class="text-xs font-bold text-slate-400 uppercase tracking-widest flex items-center">
                    <i class="fa-solid fa-chart-simple mr-2 text-cyan-500"></i>Process Metrics
                </h2>
                <div id="metrics-container" class="grid grid-cols-1 gap-3">
                    <!-- Loaded dynamically via JS -->
                    <div class="p-3 bg-slate-900/60 border border-slate-800 rounded-xl">
                        <span class="text-xs text-slate-500 block">Route Detected</span>
                        <span id="metric-route" class="text-sm font-mono text-cyan-400 truncate block">None</span>
                    </div>
                    <div class="p-3 bg-slate-900/60 border border-slate-800 rounded-xl">
                        <span class="text-xs text-slate-500 block">Data Records / Rows</span>
                        <span id="metric-rows" class="text-sm font-semibold text-slate-200">0</span>
                    </div>
                    <div class="p-3 bg-slate-900/60 border border-slate-800 rounded-xl">
                        <span class="text-xs text-slate-500 block">Scan Scope</span>
                        <span id="metric-scope" class="text-sm font-semibold text-emerald-400">N/A</span>
                    </div>
                    <div class="p-3 bg-slate-900/60 border border-slate-800 rounded-xl">
                        <span class="text-xs text-slate-500 block">Processing Time / Latency</span>
                        <span id="metric-latency" class="text-sm font-semibold text-amber-400">N/A</span>
                    </div>
                </div>
            </div>

            <!-- Navigation Panel / Table of Contents -->
            <div class="glass p-5 rounded-2xl flex-1 space-y-3 min-h-[200px]">
                <h2 class="text-xs font-bold text-slate-400 uppercase tracking-widest flex items-center">
                    <i class="fa-solid fa-bars-staggered mr-2 text-cyan-500"></i>Outline
                </h2>
                <ul id="toc-list" class="space-y-2 text-sm text-slate-400">
                    <!-- Populated dynamically -->
                </ul>
            </div>
        </aside>

        <!-- Right Main Viewport -->
        <main class="flex-1 p-6 lg:p-8 overflow-y-auto">
            
            <!-- Rendered View Section -->
            <section id="rendered-view" class="max-w-4xl mx-auto glass p-6 lg:p-10 rounded-3xl shadow-2xl relative">
                <div id="markdown-content" class="prose-custom">
                    <!-- Markdown rendered here -->
                </div>
            </section>

            <!-- Raw View Section (hidden by default) -->
            <section id="raw-view" class="max-w-5xl mx-auto hidden">
                <div class="glass rounded-3xl overflow-hidden border border-slate-800 shadow-2xl">
                    <div class="bg-slate-900/90 px-6 py-3 border-b border-slate-800 flex justify-between items-center">
                        <span class="text-xs font-mono text-slate-500">Raw Source Code</span>
                        <button id="btn-copy" class="text-xs bg-slate-800 hover:bg-slate-700 text-slate-300 px-3 py-1.5 rounded-md transition-all">
                            <i class="fa-regular fa-copy mr-1"></i> Copy Code
                        </button>
                    </div>
                    <pre class="m-0 p-6 overflow-auto text-sm leading-relaxed max-h-[80vh] font-mono"><code id="raw-code" class="hljs"></code></pre>
                </div>
            </section>
        </main>
    </div>

    <!-- Scripts Section -->
    <script>
        const rawContent = `{escaped_content}`;

        // Initialize Mermaid
        mermaid.initialize({{ startOnLoad: false, theme: 'dark' }});

        // Tab Switching Logic
        const btnRender = document.getElementById('btn-render');
        const btnRaw = document.getElementById('btn-raw');
        const renderedView = document.getElementById('rendered-view');
        const rawView = document.getElementById('raw-view');

        btnRender.addEventListener('click', () => {{
            btnRender.classList.add('bg-cyan-600', 'text-white');
            btnRender.classList.remove('bg-slate-800', 'text-slate-300');
            btnRaw.classList.add('bg-slate-800', 'text-slate-300');
            btnRaw.classList.remove('bg-cyan-600', 'text-white');
            renderedView.classList.remove('hidden');
            rawView.classList.add('hidden');
        }});

        btnRaw.addEventListener('click', () => {{
            btnRaw.classList.add('bg-cyan-600', 'text-white');
            btnRaw.classList.remove('bg-slate-800', 'text-slate-300');
            btnRender.classList.add('bg-slate-800', 'text-slate-300');
            btnRender.classList.remove('bg-cyan-600', 'text-white');
            rawView.classList.remove('hidden');
            renderedView.classList.add('hidden');
        }});

        // Copy Code Logic
        document.getElementById('btn-copy').addEventListener('click', () => {{
            navigator.clipboard.writeText(rawContent);
            const copyBtn = document.getElementById('btn-copy');
            copyBtn.innerHTML = '<i class="fa-solid fa-check mr-1 text-emerald-400"></i> Copied!';
            setTimeout(() => {{
                copyBtn.innerHTML = '<i class="fa-regular fa-copy mr-1"></i> Copy Code';
            }}, 2000);
        }});

        // Parse and Render Markdown
        const mdHtml = marked.parse(rawContent);
        document.getElementById('markdown-content').innerHTML = mdHtml;

        // Render Mermaid Diagrams
        document.querySelectorAll('pre code.language-mermaid').forEach((block, idx) => {{
            const code = block.textContent;
            const parent = block.parentElement;
            const id = 'mermaid-' + idx;
            const div = document.createElement('div');
            div.id = id;
            div.className = 'mermaid my-6 flex justify-center p-6 bg-slate-900/40 rounded-2xl border border-slate-800/80';
            div.textContent = code;
            parent.replaceWith(div);
        }});
        
        // Actually run mermaid rendering
        try {{
            mermaid.contentLoaded();
        }} catch(e) {{
            console.error('Mermaid render error:', e);
        }}

        // Syntax highlighting for raw code
        const codeElement = document.getElementById('raw-code');
        codeElement.textContent = rawContent;
        hljs.highlightElement(codeElement);

        // Highlight code blocks inside the rendered markdown as well
        document.querySelectorAll('pre code').forEach((block) => {{
            if (!block.classList.contains('language-mermaid')) {{
                hljs.highlightElement(block);
            }}
        }});

        // Extract Metrics & TOC
        function extractMetricsAndOutline(text) {{
            // Find Headings for TOC
            const headingRegex = /^#{{1,3}}\s+(.+)$/gm;
            let match;
            const tocList = document.getElementById('toc-list');
            tocList.innerHTML = '';
            
            let count = 0;
            while ((match = headingRegex.exec(text)) !== null && count < 10) {{
                const title = match[1];
                const li = document.createElement('li');
                li.className = 'hover:text-cyan-400 transition-colors cursor-pointer truncate';
                li.innerHTML = `<i class="fa-solid fa-chevron-right text-[10px] mr-1.5 text-cyan-500/60"></i> ${{title}}`;
                // Add click scroll behavior
                li.addEventListener('click', () => {{
                    const headings = Array.from(document.querySelectorAll('h1, h2, h3'));
                    const target = headings.find(h => h.textContent.includes(title));
                    if (target) {{
                        target.scrollIntoView({{ behavior: 'smooth', block: 'start' }});
                    }}
                }});
                tocList.appendChild(li);
                count++;
            }}
            if (count === 0) {{
                tocList.innerHTML = '<li class="text-xs italic text-slate-600">No outline available</li>';
            }}

            // Analyze route pattern
            const routeMatch = text.match(/(?:route|path|endpoint)\s*[:=]\s*`?([^\s\n`]+)`?/i);
            if (routeMatch) {{
                document.getElementById('metric-route').textContent = routeMatch[1];
            }}

            // Analyze latency/time pattern
            const latencyMatch = text.match(/(?:time|latency|duration|took)\s*[:=]\s*`?([0-9\.]+\s*(?:ms|s|seconds|minutes))`?/i);
            if (latencyMatch) {{
                document.getElementById('metric-latency').textContent = latencyMatch[1];
            }}

            // Analyze scan scope pattern
            const scopeMatch = text.match(/(?:scope|scan scope)\s*[:=]\s*`?([^\s\n`]+)`?/i);
            if (scopeMatch) {{
                document.getElementById('metric-scope').textContent = scopeMatch[1];
            }}

            // Count rows/records in markdown tables
            let rowCount = 0;
            const tableLines = text.split('\n');
            tableLines.forEach(line => {{
                // Markdown table rows start and end with | or contain at least two |
                if ((line.match(/\|/g) || []).length >= 2) {{
                    // Skip divider rows (e.g. |---|---|)
                    if (!line.includes('-') || line.replace(/[\|:\s-]/g, '').length > 0) {{
                        rowCount++;
                    }}
                }}
            }});
            // Subtract header and separator rows if there's a table
            if (rowCount > 2) {{
                rowCount = rowCount - 2;
            }}
            document.getElementById('metric-rows').textContent = rowCount;
        }}

        extractMetricsAndOutline(rawContent);
    </script>
</body>
</html>"#
    )
}
