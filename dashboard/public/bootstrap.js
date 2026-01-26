// insert custom scripts for analytics etc here
console.log("Bootstrap setup completed.");

// DoubleWord brand colors from the dashboard theme
const colors = {
  primary: "#2563eb",
  primaryLight: "rgba(37, 99, 235, 0.1)",
  primaryLighter: "rgba(37, 99, 235, 0.05)",
  primaryBorder: "rgba(37, 99, 235, 0.3)",
  primaryBorderLight: "rgba(37, 99, 235, 0.2)",
  background: "#ffffff",
  backgroundSecondary: "#fafaf9",
  foreground: "#2e2c26",
  muted: "#938f78",
  border: "#e2e0d3",
};

var bootstrapContent = `
  <style>
    .dw-bootstrap-banner {
      position: relative;
      border-radius: 0.75rem;
      border: 1px solid ${colors.primaryBorder};
      background: ${colors.background};
      padding: 1.5rem;
      overflow: hidden;
      font-family: 'Space Grotesk', ui-sans-serif, system-ui, sans-serif;
    }
    .dw-bootstrap-banner * {
      box-sizing: border-box;
    }
    .dw-glow-top {
      position: absolute;
      top: 0;
      right: 0;
      width: 16rem;
      height: 16rem;
      background: ${colors.primaryLighter};
      border-radius: 9999px;
      filter: blur(48px);
      transform: translate(50%, -50%);
      pointer-events: none;
    }
    .dw-glow-bottom {
      position: absolute;
      bottom: 0;
      left: 0;
      width: 12rem;
      height: 12rem;
      background: ${colors.primaryLight};
      border-radius: 9999px;
      filter: blur(32px);
      transform: translate(-25%, 50%);
      pointer-events: none;
    }
    .dw-content {
      position: relative;
      display: flex;
      flex-direction: column;
      gap: 1rem;
    }
    .dw-heading {
      font-size: 1.25rem;
      font-weight: 600;
      color: ${colors.foreground};
      margin: 0 0 0.5rem 0;
      display: flex;
      align-items: center;
      gap: 0.5rem;
    }
    .dw-pulse-dot {
      display: inline-block;
      width: 0.5rem;
      height: 0.5rem;
      border-radius: 9999px;
      background: ${colors.primary};
      animation: dwPulse 2s ease-in-out infinite;
    }
    @keyframes dwPulse {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.3; }
    }
    .dw-description {
      color: ${colors.muted};
      font-size: 0.875rem;
      margin: 0;
      line-height: 1.5;
    }
    .dw-highlight {
      color: ${colors.primary};
      font-weight: 500;
    }
    .dw-badges {
      display: flex;
      flex-wrap: wrap;
      gap: 0.75rem;
    }
    .dw-badge {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      border-radius: 9999px;
      background: rgba(255, 255, 255, 0.8);
      backdrop-filter: blur(4px);
      border: 1px solid ${colors.primaryBorderLight};
      padding: 0.375rem 0.75rem;
      font-size: 0.875rem;
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.05);
      transition: border-color 0.2s;
    }
    .dw-badge:hover {
      border-color: rgba(37, 99, 235, 0.4);
    }
    .dw-badge svg {
      width: 1rem;
      height: 1rem;
      color: ${colors.primary};
    }
    .dw-cards {
      display: grid;
      grid-template-columns: repeat(1, 1fr);
      gap: 0.75rem;
      margin-top: 0.5rem;
    }
    @media (min-width: 768px) {
      .dw-cards {
        grid-template-columns: repeat(3, 1fr);
      }
    }
    .dw-card {
      position: relative;
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      border-radius: 0.5rem;
      border: 1px solid ${colors.border};
      background: rgba(255, 255, 255, 0.7);
      backdrop-filter: blur(4px);
      padding: 1rem;
      text-decoration: none;
      transition: border-color 0.2s;
    }
    .dw-card:hover {
      border-color: rgba(37, 99, 235, 0.5);
    }
    .dw-card-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
    }
    .dw-card-header svg {
      width: 1.25rem;
      height: 1.25rem;
      color: ${colors.primary};
    }
    .dw-card-arrow {
      font-size: 0.75rem;
      color: ${colors.muted};
      opacity: 0;
      transition: opacity 0.2s;
    }
    .dw-card:hover .dw-card-arrow {
      opacity: 1;
    }
    .dw-card-title {
      font-weight: 500;
      font-size: 0.875rem;
      color: ${colors.foreground};
      margin: 0;
      transition: color 0.2s;
    }
    .dw-card:hover .dw-card-title {
      color: ${colors.primary};
    }
    .dw-card-desc {
      font-size: 0.75rem;
      color: ${colors.muted};
      margin: 0.125rem 0 0 0;
    }

  </style>
  <div class="dw-bootstrap-banner">
    <div class="dw-glow-top"></div>
    <div class="dw-glow-bottom"></div>
    <div class="dw-content">
      <div>
        <h2 class="dw-heading"><span class="dw-pulse-dot"></span>Async inference for workloads at scale</h2>
        <p class="dw-description">Run batch inference at up to <span class="dw-highlight">10x lower cost</span> than real-time APIs. Perfect for data processing, content generation, model evals, and any task that doesn't need an immediate response.</p>
      </div>
      <div class="dw-badges">
        <div class="dw-badge">
          <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="10"></circle>
            <polyline points="12 6 12 12 16 14"></polyline>
          </svg>
          <span>1h or 24h SLA</span>
        </div>
        <div class="dw-badge">
          <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="22 17 13.5 8.5 8.5 13.5 2 7"></polyline>
            <polyline points="16 17 22 17 22 11"></polyline>
          </svg>
          <span>Up to 10x cheaper</span>
        </div>
        <div class="dw-badge">
          <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path>
            <polyline points="7 10 12 15 17 10"></polyline>
            <line x1="12" x2="12" y1="15" y2="3"></line>
          </svg>
          <span>Stream results as ready</span>
        </div>
      </div>
      <div class="dw-cards">
        <a href="https://docs.doubleword.ai/batches/getting-started-with-batched-api" target="_blank" rel="noopener noreferrer" class="dw-card">
          <div class="dw-card-header">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M12 7v14"></path>
              <path d="M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"></path>
            </svg>
            <span class="dw-card-arrow">→</span>
          </div>
          <div>
            <h3 class="dw-card-title">Getting Started</h3>
            <p class="dw-card-desc">Learn how to run your first batch job</p>
          </div>
          </a>
        <a href="https://github.com/doublewordai/autobatcher" target="_blank" rel="noopener noreferrer" class="dw-card">
          <div class="dw-card-header">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"></path>
            </svg>
            <span class="dw-card-arrow">→</span>
          </div>
          <div>
            <h3 class="dw-card-title">Coming from Real-Time?</h3>
            <p class="dw-card-desc">Use Autobatcher to migrate existing API calls</p>
          </div>
          </a>
        <a href="https://github.com/doublewordai/Unsplash-Image-Summarizer-Demo" target="_blank" rel="noopener noreferrer" class="dw-card">
          <div class="dw-card-header">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <rect width="18" height="18" x="3" y="3" rx="2" ry="2"></rect>
              <circle cx="9" cy="9" r="2"></circle>
              <path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21"></path>
            </svg>
            <span class="dw-card-arrow">→</span>
          </div>
          <div>
            <h3 class="dw-card-title">Large Scale Image Processing</h3>
            <p class="dw-card-desc">See batch inference in action</p>
          </div>
          </a>
      </div>
    </div>
  </div>
`;
