import express from 'express';
import { YaamEngineClient } from './engine-client.js';

let app: express.Express | null = null;
let server: any = null;

const HTML_CONTENT = `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
    <title>YAAM Graph Visualizer</title>
    <script src="https://cdnjs.cloudflare.com/ajax/libs/cytoscape/3.26.0/cytoscape.min.js"></script>
    <style>
        :root {
            --bg-color: hsl(220, 20%, 8%);
            --panel-bg: hsla(220, 20%, 15%, 0.6);
            --panel-border: hsla(220, 20%, 30%, 0.4);
            --text-main: hsl(0, 0%, 95%);
            --text-muted: hsl(0, 0%, 70%);
            --accent: hsl(260, 100%, 70%);
        }
        * { box-sizing: border-box; }
        body {
            margin: 0; padding: 0;
            background-color: var(--bg-color);
            color: var(--text-main);
            font-family: 'Inter', -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            overflow: hidden;
            height: 100vh; height: 100dvh;
            width: 100vw;
        }
        #cy {
            width: 100%; height: 100%;
            position: absolute; top: 0; left: 0; z-index: 1;
        }
        .glass-panel {
            position: absolute; z-index: 10;
            background: var(--panel-bg);
            backdrop-filter: blur(12px);
            -webkit-backdrop-filter: blur(12px);
            border: 1px solid var(--panel-border);
            border-radius: 16px;
            box-shadow: 0 8px 32px 0 rgba(0, 0, 0, 0.4);
            transition: all 0.3s cubic-bezier(0.25, 0.8, 0.25, 1);
        }
        #sidebar {
            top: 12px; left: 12px;
            width: 280px;
            max-height: calc(100vh - 24px); max-height: calc(100dvh - 24px);
            overflow-y: auto;
            padding: 16px;
            display: flex; flex-direction: column; gap: 12px;
            padding-top: max(16px, env(safe-area-inset-top, 0px));
        }
        #sidebar.collapsed {
            width: 52px; height: 52px; overflow: hidden; padding: 0;
            top: max(12px, env(safe-area-inset-top, 12px));
            border-radius: 14px;
        }
        #sidebar.collapsed .sidebar-body { display: none; }
        #sidebar.collapsed .collapse-sidebar { display: flex; }
        .sidebar-header { display: flex; align-items: center; justify-content: space-between; }
        h1 { margin: 0; font-size: 20px; font-weight: 700;
            background: linear-gradient(135deg, var(--text-main), var(--accent));
            -webkit-background-clip: text; -webkit-text-fill-color: transparent;
            letter-spacing: -0.5px; }
        p.subtitle { margin: 0; font-size: 12px; color: var(--text-muted); line-height: 1.4; }
        .collapse-btn {
            background: hsla(0,0%,100%,0.08); border: 1px solid hsla(0,0%,100%,0.15);
            color: var(--text-muted); border-radius: 8px;
            width: 28px; height: 28px; font-size: 16px; cursor: pointer;
            display: flex; align-items: center; justify-content: center;
            transition: all 0.2s; flex-shrink: 0;
        }
        .collapse-btn:hover { background: hsla(0,0%,100%,0.15); color: var(--text-main); }
        .collapse-sidebar { display: none; width: 100%; height: 100%;
            align-items: center; justify-content: center; cursor: pointer;
            font-size: 22px; color: var(--text-muted); }
        #sidebar.collapsed .collapse-sidebar {
            background: var(--accent); color: #fff; border-radius: 14px;
        }
        .section-header { display: flex; align-items: center; justify-content: space-between; }
        .legend-title { font-size: 11px; text-transform: uppercase; letter-spacing: 1px;
            color: var(--text-muted); font-weight: 600; }
        .section-content { overflow: hidden; transition: max-height 0.3s ease, opacity 0.3s ease; }
        .section-content.collapsed { max-height: 0; opacity: 0; }
        .section-content.expanded { max-height: 600px; opacity: 1; }
        .filter-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 6px 12px; margin-top: 8px; }
        .filter-item { display: flex; align-items: center; gap: 6px; font-size: 13px; cursor: pointer; user-select: none; }
        .filter-item input { accent-color: var(--accent); width: 14px; height: 14px; cursor: pointer; margin: 0; }
        .cat-btn { background: hsla(0,0%,100%,0.08); border: 1px solid hsla(0,0%,100%,0.15);
            color: var(--text-muted); border-radius: 8px; padding: 4px 12px;
            font-size: 12px; cursor: pointer; transition: all 0.2s; font-family: inherit; }
        .cat-btn:hover { background: hsla(0,0%,100%,0.15); color: var(--text-main); }
        .cat-btn.active { background: var(--accent); color: #fff; border-color: var(--accent); }
        .legend-color { width: 12px; height: 12px; border-radius: 50%; box-shadow: 0 0 8px currentColor; flex-shrink: 0; }
        .legend-edge { width: 18px; height: 2px; border-radius: 2px; flex-shrink: 0; }
        .legend-item { display: flex; align-items: center; gap: 8px; font-size: 13px; }
        .divider { height: 1px; background: hsla(0,0%,100%,0.06); margin: 4px 0; }
        #loading {
            position: absolute; top: 50%; left: 50%; transform: translate(-50%, -50%);
            z-index: 20; font-size: 16px; font-weight: 600; color: var(--accent);
            display: flex; align-items: center; gap: 12px;
            animation: pulse 2s infinite ease-in-out;
        }
        @keyframes pulse { 0%,100% { opacity:1; } 50% { opacity:0.5; } }
        .spinner { width: 22px; height: 22px;
            border: 3px solid rgba(255,255,255,0.1); border-top-color: var(--accent);
            border-radius: 50%; animation: spin 1s linear infinite; }
        @keyframes spin { 100% { transform: rotate(360deg); } }
        #node-details {
            position: absolute; top: 12px; right: 12px;
            width: 280px; padding: 16px; display: none;
            flex-direction: column; gap: 10px;
            padding-top: max(16px, env(safe-area-inset-top, 0px));
        }
        #node-details h2 { margin: 0; font-size: 16px; word-break: break-all; }
        .tag { display: inline-block; padding: 3px 8px; border-radius: 16px;
            font-size: 11px; font-weight: 600; background: hsla(0,0%,100%,0.1);
            color: var(--text-main); border: 1px solid hsla(0,0%,100%,0.2); }
        pre.code-block { background: hsla(0,0%,0%,0.3); padding: 10px; border-radius: 8px;
            font-size: 11px; overflow-x: auto; color: var(--text-muted);
            border: 1px solid hsla(0,0%,100%,0.05); margin: 0; white-space: pre-wrap; }
        /* Mobile */
        @media (max-width: 640px) {
            #sidebar {
                width: calc(100vw - 24px); max-width: 300px;
                left: 12px;
            }
            #sidebar.collapsed { width: 52px; height: 52px; }
            /* Node details slides from bottom on mobile */
            #node-details {
                top: auto; right: 12px; left: 12px;
                bottom: max(12px, env(safe-area-inset-bottom, 12px));
                width: calc(100vw - 24px); max-width: none;
                max-height: 50vh; max-height: 50dvh;
            }
        }
    </style>
</head>
<body>
    <div id="cy"></div>

    <div id="sidebar" class="glass-panel">
        <div class="collapse-sidebar" onclick="toggleSidebar()">☰</div>
        <div class="sidebar-body">
            <div class="sidebar-header">
                <h1>YAAM</h1>
                <button class="collapse-btn" onclick="toggleSidebar()">−</button>
            </div>
            <p class="subtitle">Code topology & memory graph</p>

            <div class="divider"></div>

            <div class="section-header">
                <span class="legend-title">Filters</span>
                <button class="collapse-btn" onclick="toggleSection('filter-content', this)">−</button>
            </div>
            <div id="filter-content" class="section-content expanded">
                <div id="category-filters" style="display:flex;gap:6px;flex-wrap:wrap;margin-bottom:8px;"></div>
                <div class="divider"></div>
                <div class="filter-grid" id="edge-filters"></div>
            </div>

            <div class="divider"></div>

            <div class="section-header">
                <span class="legend-title">Legend</span>
                <button class="collapse-btn" onclick="toggleSection('legend-content', this)">−</button>
            </div>
            <div id="legend-content" class="section-content expanded">
                <div class="legend-title">Nodes</div>
                <div id="legend-nodes" style="display:flex;flex-wrap:wrap;gap:6px 12px;margin-top:4px;"></div>
                <div class="divider"></div>
                <div class="legend-title">Edges</div>
                <div id="legend-edges" style="display:flex;flex-direction:column;gap:6px;margin-top:4px;"></div>
            </div>
        </div>
    </div>

    <div id="node-details" class="glass-panel">
        <div class="sidebar-header">
            <h2 id="nd-title">Node</h2>
            <button class="collapse-btn" onclick="document.getElementById('node-details').style.display='none'">×</button>
        </div>
        <div><span id="nd-type" class="tag">Type</span></div>
        <p id="nd-desc" class="subtitle"></p>
        <pre id="nd-meta" class="code-block"></pre>
    </div>

    <div id="loading"><div class="spinner"></div> Loading topology...</div>

    <script>
        const COLORS = {
            Workspace: 'hsl(280, 80%, 65%)',
            File: 'hsl(210, 80%, 60%)',
            Function: 'hsl(150, 80%, 55%)',
            Class: 'hsl(180, 80%, 55%)',
            Scratchpad: 'hsl(40, 80%, 60%)',
            Default: 'hsl(0, 0%, 60%)'
        };
        const EDGE_COLORS = {
            CALLS: 'hsl(0, 80%, 65%)',
            MAPPED_TO: 'hsl(280, 80%, 65%)',
            IMPORTS: 'hsl(150, 80%, 55%)',
            DECLARED_IN: 'hsla(0, 0%, 50%, 0.4)',
            HAS_SCRATCHPAD: 'hsl(40, 80%, 60%)'
        };
        const SIZES = {
            Workspace: 70, File: 45, Function: 28, Class: 35, Scratchpad: 28, Default: 28
        };
        const NODE_TYPES = ['Workspace','File','Function','Class','Scratchpad'];
        const EDGE_TYPES = ['CALLS','MAPPED_TO','IMPORTS','DECLARED_IN','HAS_SCRATCHPAD'];

        let cy;
        const CATEGORIES = ['all','project','library'];
        let activeCategory = 'all';
        const activeEdgeFilters = new Set(EDGE_TYPES);

        function toggleSidebar() {
            const sb = document.getElementById('sidebar');
            sb.classList.toggle('collapsed');
        }
        function toggleSection(id, btn) {
            const el = document.getElementById(id);
            el.classList.toggle('collapsed');
            el.classList.toggle('expanded');
            btn.textContent = el.classList.contains('collapsed') ? '+' : '−';
        }

        function deriveCategory(id) {
            const LIB = ['node_modules/','.venv/','site-packages/','/dist/','/build/','/target/'];
            if (LIB.some(m => id.includes(m))) return 'library';
            return 'project';
        }

        function applyFilters() {
            if (!cy) return;
            cy.nodes().forEach(node => {
                const cat = node.data('category') || 'project';
                if (activeCategory === 'all' || cat === activeCategory) node.show();
                else node.hide();
            });
            cy.edges().forEach(edge => {
                if (!activeEdgeFilters.has(edge.data('label'))) { edge.hide(); return; }
                if (activeCategory === 'all') { edge.show(); return; }
                const srcCat = edge.source().data('category') || 'project';
                const tgtCat = edge.target().data('category') || 'project';
                if (srcCat === activeCategory && tgtCat === activeCategory) edge.show();
                else edge.hide();
            });
        }

        // Build filter + legend UI
        function buildControls() {
            // Category filter buttons
            const cf = document.getElementById('category-filters');
            CATEGORIES.forEach(cat => {
                const label = cat.charAt(0).toUpperCase() + cat.slice(1);
                const btn = document.createElement('button');
                btn.className = 'cat-btn' + (cat === 'all' ? ' active' : '');
                btn.textContent = label;
                btn.onclick = () => {
                    activeCategory = cat;
                    document.querySelectorAll('.cat-btn').forEach(b => b.classList.remove('active'));
                    btn.classList.add('active');
                    applyFilters();
                };
                cf.appendChild(btn);
            });
            // Edge filters
            const ef = document.getElementById('edge-filters');
            const le = document.getElementById('legend-edges');
            const ln = document.getElementById('legend-nodes');
            // Node legend (colors only, not filters)
            NODE_TYPES.forEach(t => {
                const c = COLORS[t] || COLORS.Default;
                ln.innerHTML += '<div class="legend-item"><div class="legend-color" style="color:'+c+';background:'+c+'"></div>'+t+'</div>';
            });
            EDGE_TYPES.forEach(t => {
                const c = EDGE_COLORS[t] || '#555';
                const dash = t === 'DECLARED_IN' ? 'border-top: 2px dashed '+c+'; background: none;' : 'background: '+c+';';
                ef.innerHTML += '<label class="filter-item"><input type="checkbox" data-kind="edge" data-type="'+t+'" checked><div class="legend-edge" style="'+dash+'"></div>'+t+'</label>';
                le.innerHTML += '<div class="legend-item"><div class="legend-edge" style="'+dash+'"></div>'+t+'</div>';
            });
            document.querySelectorAll('input[data-kind="edge"]').forEach(cb => {
                cb.addEventListener('change', () => {
                    const type = cb.dataset.type;
                    if (cb.checked) activeEdgeFilters.add(type); else activeEdgeFilters.delete(type);
                    applyFilters();
                });
            });
        }

        async function initGraph() {
            try {
                const res = await fetch('/api/graph');
                const data = await res.json();
                document.getElementById('loading').style.display = 'none';

                cy = cytoscape({
                    container: document.getElementById('cy'),
                    elements: data,
                    style: [
                        { selector: 'node', style: {
                            'label': 'data(name)', 'color': '#fff',
                            'text-valign': 'bottom', 'text-halign': 'center', 'text-margin-y': 6,
                            'font-size': '11px', 'font-family': 'Inter, sans-serif',
                            'text-outline-color': '#1a1d24', 'text-outline-width': 3,
                            'background-color': (ele) => COLORS[ele.data('type')] || COLORS.Default,
                            'width': (ele) => SIZES[ele.data('type')] || SIZES.Default,
                            'height': (ele) => SIZES[ele.data('type')] || SIZES.Default,
                            'border-width': 2, 'border-color': 'hsla(0,0%,100%,0.2)'
                        }},
                        { selector: 'node:selected', style: {
                            'border-color': '#fff', 'border-width': 4,
                            'shadow-color': (ele) => COLORS[ele.data('type')] || COLORS.Default,
                            'shadow-blur': 24, 'shadow-opacity': 0.8
                        }},
                        { selector: 'edge', style: {
                            'width': 2,
                            'line-color': (ele) => EDGE_COLORS[ele.data('label')] || '#555',
                            'target-arrow-color': (ele) => EDGE_COLORS[ele.data('label')] || '#555',
                            'target-arrow-shape': 'triangle', 'curve-style': 'bezier',
                            'opacity': 0.6, 'font-size': '9px', 'color': '#aaa',
                            'text-outline-color': '#1a1d24', 'text-outline-width': 2
                        }},
                        { selector: 'edge[label = "DECLARED_IN"]', style: {
                            'width': 1, 'line-style': 'dashed', 'target-arrow-shape': 'none'
                        }},
                        { selector: 'edge:selected', style: {
                            'width': 4, 'opacity': 1, 'label': 'data(label)'
                        }}
                    ],
                    layout: { name: 'cose', animate: true, randomize: true,
                        nodeRepulsion: 400000, idealEdgeLength: 150, edgeElasticity: 100,
                        gravity: 0.1, padding: 50 }
                });

                cy.on('mouseover', 'node', function(e){
                    const node = e.target;
                    node.style('shadow-color', COLORS[node.data('type')] || COLORS.Default);
                    node.style('shadow-blur', 16); node.style('shadow-opacity', 0.8);
                    document.body.style.cursor = 'pointer';
                });
                cy.on('mouseout', 'node', function(e){
                    if(!e.target.selected()) e.target.style('shadow-opacity', 0);
                    document.body.style.cursor = 'default';
                });
                cy.on('tap', 'node', function(e){
                    const d = e.target.data();
                    const panel = document.getElementById('node-details');
                    panel.style.display = 'flex';
                    document.getElementById('nd-title').textContent = d.name || d.id;
                    document.getElementById('nd-type').textContent = d.type || 'Unknown';
                    document.getElementById('nd-type').style.background = COLORS[d.type] || COLORS.Default;
                    document.getElementById('nd-desc').textContent = d.content || d.description || '';
                    const metaEl = document.getElementById('nd-meta');
                    if (d.metadata && d.metadata !== 'null') {
                        try { metaEl.textContent = JSON.stringify(JSON.parse(d.metadata), null, 2); metaEl.style.display = 'block'; }
                        catch(err) { metaEl.textContent = String(d.metadata); metaEl.style.display = 'block'; }
                    } else { metaEl.style.display = 'none'; }
                });
                cy.on('tap', function(e){
                    if(e.target === cy) document.getElementById('node-details').style.display = 'none';
                });

                buildControls();
                applyFilters();

                // Auto-collapse sidebar on mobile
                if (window.innerWidth <= 640) toggleSidebar();

            } catch (err) {
                document.getElementById('loading').innerHTML = '<span style="color:hsl(0,80%,60%)">Failed to load graph.</span>';
            }
        }
        initGraph();
    </script>
</body>
</html>`;

export async function startServerIfNeeded(client: YaamEngineClient, port: number = 3456): Promise<string> {
    if (server) return `http://localhost:${port}`;
    
    app = express();
    app.use(express.json());

    app.get('/', (req, res) => {
        res.send(HTML_CONTENT);
    });

    app.get('/api/graph', async (req, res) => {
        try {
            const rawNodes = await client.query({ match: {} });
            
            const deriveCategory = (id: string): string => {
                const LIB = ['node_modules/','.venv/','site-packages/','/dist/','/build/','/target/'];
                if (LIB.some(m => id.includes(m))) return 'library';
                return 'project';
            };
            
            const cyNodes: any[] = [];
            const nodeIds = new Set();
            
            for (const n of rawNodes) {
                // Ensure unique IDs
                if (nodeIds.has(n.id)) continue;
                nodeIds.add(n.id);
                
                let type = 'Unknown';
                if (n.label?.label === 'Entity') type = n.label?.type || 'Entity';
                else if (n.label?.label === 'Workspace') type = 'Workspace';
                else if (n.label?.label === 'Scratchpad') type = 'Scratchpad';
                
                const category = deriveCategory(n.id);
                
                cyNodes.push({
                    data: {
                        id: n.id,
                        name: n.name || n.id,
                        type: type,
                        category: category,
                        content: n.content,
                        description: n.properties?.description,
                        metadata: n.metadata
                    }
                });
            }

            const cyEdges: any[] = [];
            const edgeRels = ["CALLS", "DECLARED_IN", "IMPORTS", "MAPPED_TO", "HAS_SCRATCHPAD", "IMPLEMENTS", "INHERITS_FROM"];
            
            // Reconstruct edges via DSL traversal
            // 1 level outbound from each node for each relationship
            await Promise.all(rawNodes.map(async (n: any) => {
                for (const rel of edgeRels) {
                    try {
                        const targets = await client.query({
                            match: { id: n.id },
                            traverse: { relationship: rel, direction: "outbound", max_depth: 1 }
                        });
                        if (targets && targets.length > 0) {
                            for (const t of targets) {
                                // Skip self loops and ensure target exists
                                if (t.id !== n.id && nodeIds.has(t.id)) {
                                    cyEdges.push({
                                        data: {
                                            id: `${n.id}-${rel}-${t.id}`,
                                            source: n.id,
                                            target: t.id,
                                            label: rel
                                        }
                                    });
                                }
                            }
                        }
                    } catch (err) {
                        // ignore query error for individual traversals
                    }
                }
            }));

            res.json({
                nodes: cyNodes,
                edges: cyEdges
            });
        } catch (e: any) {
            // Graph API Error
            res.status(500).json({ error: e.message || String(e) });
        }
    });

    return new Promise((resolve, reject) => {
        server = app!.listen(port, '0.0.0.0', () => {
            // YAAM Visualizer running
            resolve(`http://localhost:${port}`);
        }).on('error', (err: any) => {
            if (err.code === 'EADDRINUSE') {
                // Already running
                resolve(`http://localhost:${port}`);
            } else {
                reject(err);
            }
        });
    });
}