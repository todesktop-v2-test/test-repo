import { defineConfig } from '@todesktop/v2-config';
import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import { licenseNotice } from './scripts/licenses.ts';

export default defineConfig({
  appId: 'acme-agent-dev',
  description: 'Sample headless daemon for the ToDesktop v2 spike',
  homepage: 'https://acme.example',
  maintainer: 'Acme <support@acme.example>',
  productName: 'Acme Agent',
  // Input: the app cargo built (see .github/workflows/release.yml).
  // Output: the formats ToDesktop makes from each.
  linux: {
    apps: { x64: 'dist/linux-x64' },
    executable: 'acme-agent',
    formats: ['deb', 'rpm', 'pacman', 'tar.gz'],
    service: { description: 'Acme Agent' },
  },
  hooks: {
    // Runs before ToDesktop signs the app, so the signature covers the file.
    prepareApp: async ({ appDir, config, platform }) => {
      const resources =
        platform === 'mac'
          ? path.join(appDir, 'Contents', 'Resources')
          : appDir;
      await writeFile(
        path.join(resources, 'THIRD-PARTY-LICENSES.txt'),
        licenseNotice(config.productName),
      );
    },
  },
});
