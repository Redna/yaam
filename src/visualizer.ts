import express from 'express';
import { YaamEngineClient } from './engine-client.js';

let app: express.Express | null = null;
let server: any = null;

const HTML_CONTENT = `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
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
        body {
            margin: 0;
            padding: 0;
            background-color: var(--bg-color);
            color: var(--text-main);
            font-family: 'Inter', -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            overflow: hidden;
            display: flex;
            height: 100vh;
            width: 100vw;
        }
        #cy {
            width: 100%;
            height: 100%;
            position: absolute;
            top: 0;
            left: 0;
            z-index: 1;
        }
        .glass-panel {
            position: absolute;
            z-index: 10;
            background: var(--panel-bg);
            backdrop-filter: blur(12px);
            -webkit-backdrop-filter: blur(12px);
            border: 1px solid var(--panel-border);
            border-radius: 16px;
            padding: 24px;
            box-shadow: 0 8px 32px 0 rgba(0, 0, 0, 0.4);
            transition: all 0.3s cubic-bezier(0.25, 0.8, 0.25, 1);
        }
        #sidebar {
            top: 24px;
            left: 24px;
            width: 320px;
            max-height: calc(100vh - 48px);
            overflow-y: auto;
            display: flex;
            flex-direction: column;
            gap: 16px;
        }
        #sidebar:hover {
            box-shadow: 0 12px 48px 0 rgba(0, 0, 0, 0.6);
            border-color: hsla(220, 20%, 40%, 0.6);
        }
        h1 {
            margin: 0;
            font-size: 24px;
            font-weight: 700;
            background: linear-gradient(135deg, var(--text-main), var(--accent));
            -webkit-background-clip: text;
            -webkit-text-fill-color: transparent;
            letter-spacing: -0.5px;
        }
        p.subtitle {
            margin: 0;
            font-size: 14px;
            color: var(--text-muted);
            line-height: 1.5;
        }
        .legend {
            margin-top: auto;
            display: flex;
            flex-direction: column;
            gap: 12px;
        }
        .legend-title {
            font-size: 12px;
            text-transform: uppercase;
            letter-spacing: 1px;
            color: var(--text-muted);
            font-weight: 600;
            margin-bottom: 4px;
        }
        .legend-item {
            display: flex;
            align-items: center;
            gap: 12px;
            font-size: 14px;
        }
        .legend-color {
            width: 16px;
            height: 16px;
            border-radius: 50%;
            box-shadow: 0 0 12px currentColor;
        }
        .legend-edge {
            width: 24px;
            height: 3px;
            border-radius: 2px;
        }
        #loading {
            position: absolute;
            top: 50%;
            left: 50%;
            transform: translate(-50%, -50%);
            z-index: 20;
            font-size: 18px;
            font-weight: 600;
            color: var(--accent);
            display: flex;
            align-items: center;
            gap: 12px;
            animation: pulse 2s infinite ease-in-out;
        }
        @keyframes pulse {
            0%, 100% { opacity: 1; transform: translate(-50%, -50%) scale(1); }
            50% { opacity: 0.6; transform: translate(-50%, -50%) scale(0.98); }
        }
        .spinner {
            width: 24px;
            height: 24px;
            border: 3px solid rgba(255,255,255,0.1);
            border-top-color: var(--accent);
            border-radius: 50%;
            animation: spin 1s linear infinite;
        }
        @keyframes spin { 100% { transform: rotate(360deg); } }
        
        #node-details {
            position: absolute;
            top: 24px;
            right: 24px;
            width: 320px;
            display: none;
            flex-direction: column;
            gap: 12px;
        }
        #node-details h2 { margin: 0; font-size: 18px; word-break: break-all; }
        .tag {
            display: inline-block;
            padding: 4px 10px;
            border-radius: 20px;
            font-size: 12px;
            font-weight: 600;
            background: hsla(0,0%,100%,0.1);
            color: var(--text-main);
            border: 1px solid hsla(0,0%,100%,0.2);
        }
        pre.code-block {
            background: hsla(0,0%,0%,0.3);
            padding: 12px;
            border-radius: 8px;
            font-size: 12px;
            overflow-x: auto;
            color: var(--text-muted);
            border: 1px solid hsla(0,0%,100%,0.05);
            margin: 0;
            white-space: pre-wrap;
        }
    </style>
</head>
<body>
    <div id="cy"></div>
    
    <div id="sidebar" class="glass-panel">
        <h1>YAAM Graph View</h1>
        <p class="subtitle">Real-time code topology & memory graph.</p>
        
        <div class="legend" style="margin-top: 24px;">
            <div class="legend-title">Nodes</div>
            <div class="legend-item"><div class="legend-color" style="color: hsl(280, 80%, 65%); background: hsl(280, 80%, 65%);"></div> Workspace</div>
            <div class="legend-item"><div class="legend-color" style="color: hsl(210, 80%, 60%); background: hsl(210, 80%, 60%);"></div> File</div>
            <div class="legend-item"><div class="legend-color" style="color: hsl(150, 80%, 55%); background: hsl(150, 80%, 55%);"></div> Function / Class</div>
            <div class="legend-item"><div class="legend-color" style="color: hsl(40, 80%, 60%); background: hsl(40, 80%, 60%);"></div> Scratchpad</div>
            
            <div class="legend-title" style="margin-top: 16px;">Edges</div>
            <div class="legend-item"><div class="legend-edge" style="background: hsl(0, 0%, 50%);"></div> DEFAULT</div>
            <div class="legend-item"><div class="legend-edge" style="background: hsl(280, 80%, 65%);"></div> MAPPED_TO</div>
            <div class="legend-item"><div class="legend-edge" style="background: hsl(0, 80%, 65%);"></div> CALLS</div>
            <div class="legend-item"><div class="legend-edge" style="background: hsl(150, 80%, 55%);"></div> IMPORTS</div>
        </div>
    </div>
    
    <div id="node-details" class="glass-panel">
        <h2 id="nd-title">Node Name</h2>
        <div><span id="nd-type" class="tag">Type</span></div>
        <p id="nd-desc" class="subtitle"></p>
        <pre id="nd-meta" class="code-block"></pre>
    </div>

    <div id="loading">
        <div class="spinner"></div> Loading topology...
    </div>

    <script>
        // Setup Cytoscape colors and styles
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
            Workspace: 80,
            File: 50,
            Function: 30,
            Class: 40,
            Scratchpad: 30,
            Default: 30
        };

        async function initGraph() {
            try {
                const res = await fetch('/api/graph');
                const data = await res.json();
                
                document.getElementById('loading').style.display = 'none';

                const cy = cytoscape({
                    container: document.getElementById('cy'),
                    elements: data,
                    style: [
                        {
                            selector: 'node',
                            style: {
                                'label': 'data(name)',
                                'color': '#fff',
                                'text-valign': 'bottom',
                                'text-halign': 'center',
                                'text-margin-y': 8,
                                'font-size': '12px',
                                'font-family': 'Inter, sans-serif',
                                'text-outline-color': '#1a1d24',
                                'text-outline-width': 3,
                                'background-color': (ele) => COLORS[ele.data('type')] || COLORS.Default,
                                'width': (ele) => SIZES[ele.data('type')] || SIZES.Default,
                                'height': (ele) => SIZES[ele.data('type')] || SIZES.Default,
                                'border-width': 2,
                                'border-color': 'hsla(0,0%,100%,0.2)',
                                'transition-property': 'background-color, transform, box-shadow',
                                'transition-duration': '0.3s'
                            }
                        },
                        {
                            selector: 'node:selected',
                            style: {
                                'border-color': '#fff',
                                'border-width': 4,
                                'shadow-color': (ele) => COLORS[ele.data('type')] || COLORS.Default,
                                'shadow-blur': 24,
                                'shadow-opacity': 0.8
                            }
                        },
                        {
                            selector: 'edge',
                            style: {
                                'width': 2,
                                'line-color': (ele) => EDGE_COLORS[ele.data('label')] || '#555',
                                'target-arrow-color': (ele) => EDGE_COLORS[ele.data('label')] || '#555',
                                'target-arrow-shape': 'triangle',
                                'curve-style': 'bezier',
                                'opacity': 0.6,
                                'font-size': '10px',
                                'color': '#aaa',
                                'text-outline-color': '#1a1d24',
                                'text-outline-width': 2,
                                'transition-property': 'opacity, width, line-color',
                                'transition-duration': '0.3s'
                            }
                        },
                        {
                            selector: 'edge[label = "DECLARED_IN"]',
                            style: {
                                'width': 1,
                                'line-style': 'dashed',
                                'target-arrow-shape': 'none'
                            }
                        },
                        {
                            selector: 'edge:selected',
                            style: {
                                'width': 4,
                                'opacity': 1,
                                'label': 'data(label)'
                            }
                        }
                    ],
                    layout: {
                        name: 'cose',
                        animate: true,
                        randomize: true,
                        nodeRepulsion: 400000,
                        idealEdgeLength: 150,
                        edgeElasticity: 100,
                        gravity: 0.1,
                        padding: 50
                    }
                });

                // Micro-animations on hover
                cy.on('mouseover', 'node', function(e){
                    const node = e.target;
                    node.style('shadow-color', COLORS[node.data('type')] || COLORS.Default);
                    node.style('shadow-blur', 16);
                    node.style('shadow-opacity', 0.8);
                    
                    document.body.style.cursor = 'pointer';
                });
                
                cy.on('mouseout', 'node', function(e){
                    const node = e.target;
                    if(!node.selected()) {
                        node.style('shadow-opacity', 0);
                    }
                    document.body.style.cursor = 'default';
                });

                // Interactive sidebar update
                cy.on('tap', 'node', function(e){
                    const node = e.target;
                    const d = node.data();
                    
                    const panel = document.getElementById('node-details');
                    panel.style.display = 'flex';
                    
                    document.getElementById('nd-title').textContent = d.name || d.id;
                    document.getElementById('nd-type').textContent = d.type || 'Unknown';
                    document.getElementById('nd-type').style.background = COLORS[d.type] || COLORS.Default;
                    
                    document.getElementById('nd-desc').textContent = d.content || d.description || '';
                    
                    const metaEl = document.getElementById('nd-meta');
                    if (d.metadata && d.metadata !== 'null') {
                        try {
                            const parsed = JSON.parse(d.metadata);
                            metaEl.textContent = JSON.stringify(parsed, null, 2);
                            metaEl.style.display = 'block';
                        } catch(err) {
                            metaEl.textContent = String(d.metadata);
                            metaEl.style.display = 'block';
                        }
                    } else {
                        metaEl.style.display = 'none';
                    }
                });

                cy.on('tap', function(e){
                    if(e.target === cy){
                        document.getElementById('node-details').style.display = 'none';
                    }
                });

            } catch (err) {
                document.getElementById('loading').innerHTML = '<span style="color:hsl(0,80%,60%)">Failed to load graph data.</span>';
                // Failed to load graph data
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
            // Fetch all nodes
            const rawNodes = await client.query({ match: {} });
            
            const cyNodes: any[] = [];
            const nodeIds = new Set();
            
            for (const n of rawNodes) {
                // Ensure unique IDs
                if (nodeIds.has(n.id)) continue;
                nodeIds.add(n.id);
                
                let type = 'Unknown';
                if (n.label === 'Entity') type = n.properties?.type || 'Entity';
                else if (n.label === 'Workspace') type = 'Workspace';
                else if (n.label === 'Scratchpad') type = 'Scratchpad';
                
                cyNodes.push({
                    data: {
                        id: n.id,
                        name: n.name || n.id,
                        type: type,
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