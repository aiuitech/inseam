import { spawn } from 'node:child_process';

export class Runner {
  child = null;
  busy = false;
  job = null;

  async exclusive(action) {
    if (this.busy) throw new Error('An operation is already running. Wait or cancel it first.');
    this.busy = true;
    try { return await action(); }
    finally { this.busy = false; }
  }

  start(action, label) {
    if (this.busy) throw new Error('An operation is already running');
    this.job = { state: 'running', label, log: '', started: new Date().toISOString() };
    void this.exclusive(action).then(result => {
      this.job = { ...this.job, state: 'completed', result };
    }, error => {
      this.job = { ...this.job, state: 'failed', error: error.message };
    });
    return this.job;
  }

  cancel() {
    this.child?.kill('SIGTERM');
  }

  command(binary, args, timeout = 600000) {
    return new Promise((resolve, reject) => {
      const child = spawn(binary, args, { shell: false, stdio: ['ignore', 'pipe', 'pipe'] });
      this.child = child;
      let output = '';
      let errors = '';
      let size = 0;
      let failure;
      const timer = setTimeout(() => { failure = 'Operation timed out'; child.kill('SIGKILL'); }, timeout);
      const append = (chunk, stderr) => {
        size += chunk.length;
        if (size > 16 * 1024 * 1024) { failure = 'Operation output exceeded 16 MiB'; child.kill('SIGKILL'); return; }
        if (stderr) errors = (errors + chunk.toString()).slice(-65536);
        else output += chunk.toString();
        if (this.job?.state === 'running') this.job.log = (this.job.log + chunk.toString()).slice(-65536);
      };
      child.stdout.on('data', chunk => append(chunk, false));
      child.stderr.on('data', chunk => append(chunk, true));
      child.on('error', reject);
      child.on('close', (code, signal) => {
        clearTimeout(timer);
        this.child = null;
        if (code === 0 && !failure) resolve(output);
        else reject(new Error(failure ?? `Command ${signal ?? code}: ${errors.slice(-4000)}`));
      });
    });
  }
}
