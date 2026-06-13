// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Imprint (Impressum) + privacy policy — German legal requirement
// (§ 5 DDG / Art. 13 DSGVO). Opened from the status strip.

import { useStore } from "../store";

export function ImprintModal() {
  const open = useStore((s) => s.imprintOpen);
  const close = useStore((s) => s.openImprint);
  if (!open) return null;
  return (
    <div className="modalback" onClick={() => close(false)}>
      <div className="modal imprint" onClick={(e) => e.stopPropagation()}>
        <div className="modalhead">
          <h2>Imprint &amp; Privacy Policy</h2>
          <button className="x" onClick={() => close(false)}>
            ×
          </button>
        </div>

        <h3>Imprint (Impressum)</h3>
        <p>
          CNC Kitchen
          <br />
          Stefan Hermann
          <br />
          Bahnhofstr. 2
          <br />
          88145 Hergatz
          <br />
          Germany
        </p>
        <p>
          Email: <a href="mailto:contact@cnckitchen.com">contact@cnckitchen.com</a>
          <br />
          Phone: <a href="tel:+491752011824">+49 175 2011824</a>
          <br />
          <span className="dim">
            The phone number is for legal/business inquiries only — not for support.
          </span>
        </p>
        <p>
          EU Online Dispute Resolution platform:{" "}
          <a href="https://ec.europa.eu/consumers/odr" target="_blank" rel="noopener noreferrer">
            ec.europa.eu/consumers/odr
          </a>
        </p>

        <h3>Privacy Policy (Datenschutzerklärung)</h3>
        <p>
          Responsible party (Verantwortlicher gem. Art. 4 Abs. 7 DSGVO): Stefan Hermann,
          Bahnhofstr. 2, 88145 Hergatz, Germany.
        </p>
        <ul>
          <li>
            This website is hosted on GitHub Pages (GitHub Inc. / Microsoft Corp., 88 Colin P
            Kelly Jr St, San Francisco, CA 94107, USA). When you visit this site, GitHub may
            process your IP address in server logs. Legal basis: Art. 6(1)(f) DSGVO (legitimate
            interest in providing the website). See{" "}
            <a
              href="https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement"
              target="_blank"
              rel="noopener noreferrer"
            >
              GitHub’s Privacy Statement
            </a>
            .
          </li>
          <li>
            This tool stores user preferences and project data (materials, pattern curves,
            settings, auto-saved projects) in your browser’s localStorage and IndexedDB. Your
            models and all analysis run locally — this data never leaves your device and is not
            transmitted to any server.
          </li>
          <li>This website does not use cookies, analytics, or any tracking technologies.</li>
          <li>
            This site contains links to external websites (e.g., GitHub, CNCKitchen.STORE,
            PayPal, Ko-fi). These sites have their own privacy policies, over which we have no
            control.
          </li>
          <li>
            Under the GDPR you have the right to access, rectification, erasure, restriction of
            processing, data portability, and the right to lodge a complaint with a supervisory
            authority.
          </li>
        </ul>
      </div>
    </div>
  );
}
