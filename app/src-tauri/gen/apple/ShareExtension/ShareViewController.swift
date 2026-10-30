import UIKit
import UniformTypeIdentifiers

/// Share Extension: takes the link (or text containing a link) from the share sheet and hands it to the Marginalia app
/// as marginalia://share?url=<percent-encoded link>. The app (tauri-plugin-mobile-sharetarget + tauri-plugin-deep-link)
/// queues it, saves it through the server's API and shows the confirmation. No UI of its own beyond a spinner.
class ShareViewController: UIViewController {
    private let scheme = "marginalia"

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        let spinner = UIActivityIndicatorView(style: .large)
        spinner.center = view.center
        spinner.startAnimating()
        view.addSubview(spinner)
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        extractSharedURL { [weak self] url in
            guard let self = self else { return }
            guard let url = url, let deepLink = self.deepLink(for: url) else {
                self.showErrorAndClose("No web link found in what was shared.")
                return
            }
            if self.openInHostApp(deepLink) {
                // Give the system a moment to switch to the app before the extension goes away.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { self.close() }
            } else {
                self.showErrorAndClose("Could not open Marginalia.")
            }
        }
    }

    private func deepLink(for url: URL) -> URL? {
        // Encode everything but letters and digits, so '&', '=', '#' and '+' in the link cannot be misread.
        guard let encoded = url.absoluteString.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { return nil }
        return URL(string: "\(scheme)://share?url=\(encoded)")
    }

    private func close() {
        extensionContext?.completeRequest(returningItems: [], completionHandler: nil)
    }

    private func showErrorAndClose(_ message: String) {
        let alert = UIAlertController(title: "Marginalia", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in self.close() })
        present(alert, animated: true)
    }

    /// A share extension cannot call UIApplication.shared; the responder chain still reaches the host's UIApplication.
    private func openInHostApp(_ url: URL) -> Bool {
        var responder: UIResponder? = self
        while let current = responder {
            if let application = current as? UIApplication {
                application.open(url, options: [:], completionHandler: nil)
                return true
            }
            responder = current.next
        }
        return false
    }

    // MARK: - Reading what was shared

    private func firstURL(in text: String) -> URL? {
        let detector = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)
        let range = NSRange(location: 0, length: (text as NSString).length)
        if let match = detector?.firstMatch(in: text, options: [], range: range), let url = match.url,
           let scheme = url.scheme?.lowercased(), scheme == "http" || scheme == "https" {
            return url
        }
        return nil
    }

    private func extractSharedURL(completion: @escaping (URL?) -> Void) {
        let providers = (extensionContext?.inputItems as? [NSExtensionItem])?.flatMap { $0.attachments ?? [] } ?? []

        func finish(_ url: URL?) { DispatchQueue.main.async { completion(url) } }

        // 1. A real URL item (Safari, most apps).
        if let provider = providers.first(where: { $0.hasItemConformingToTypeIdentifier(UTType.url.identifier) }) {
            provider.loadItem(forTypeIdentifier: UTType.url.identifier, options: nil) { item, _ in
                if let url = item as? URL, url.scheme?.hasPrefix("http") == true { return finish(url) }
                if let string = item as? String { return finish(self.firstURL(in: string)) }
                finish(nil)
            }
            return
        }
        // 2. Text that contains a link.
        if let provider = providers.first(where: { $0.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) }) {
            provider.loadItem(forTypeIdentifier: UTType.plainText.identifier, options: nil) { item, _ in
                if let string = item as? String { return finish(self.firstURL(in: string)) }
                if let data = item as? Data, let string = String(data: data, encoding: .utf8) { return finish(self.firstURL(in: string)) }
                finish(nil)
            }
            return
        }
        finish(nil)
    }
}
