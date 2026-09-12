const toast = (message) => {
  const el = document.getElementById('toast');
  const text = document.getElementById('toastText');
  if (!el || !text) return;
  text.textContent = message;
  el.classList.add('show');
  window.setTimeout(() => el.classList.remove('show'), 2800);
};

// Crow is driven by GitHub events. The dashboard stays read-only and lets a
// developer inspect the latest automatic review without starting one by hand.
document.querySelectorAll('.finding').forEach((card) => {
  card.addEventListener('click', () => {
    document.querySelectorAll('.finding').forEach((item) => item.classList.remove('selected-finding'));
    card.classList.add('selected-finding');
  });
});

document.querySelectorAll('.tab').forEach((tab) => {
  tab.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach((item) => item.classList.remove('active'));
    tab.classList.add('active');
    if (tab.dataset.tab !== 'overview') toast(`${tab.textContent.trim()} history is coming next`);
  });
});

document.querySelectorAll('.recent-item').forEach((item) => {
  item.addEventListener('click', () => {
    document.querySelectorAll('.recent-item').forEach((entry) => entry.classList.remove('selected'));
    item.classList.add('selected');
    toast(`Opening ${item.querySelector('strong')?.textContent || 'review'}`);
  });
});

document.querySelector('.provider-chip')?.addEventListener('click', () => toast('Provider is configured on the Crow server'));
document.querySelector('.icon-btn[title="Search"]')?.addEventListener('click', () => toast('Search across recent reviews'));
document.querySelector('.icon-btn[title="Help"]')?.addEventListener('click', () => toast('Crow reviews PRs automatically when GitHub sends an update'));
document.querySelectorAll('.inline-comment').forEach((button) => button.addEventListener('click', (event) => { event.stopPropagation(); toast('Open the linked review on GitHub'); }));
