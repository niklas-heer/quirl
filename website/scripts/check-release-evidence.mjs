import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  loadEvidence,
  validateRepositoryProjections,
} from './release-evidence.mjs';

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(websiteRoot, '..');
const { metadata } = loadEvidence(repositoryRoot);
validateRepositoryProjections(repositoryRoot, metadata);
console.log(`Checked ${metadata.status} release evidence attribution and projections.`);
