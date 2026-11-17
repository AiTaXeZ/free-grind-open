import AuthenticationServices
import SwiftRs
import Tauri
import UIKit
import WebKit

private class NoInputAccessoryView: UIView {
    override var inputAccessoryView: UIView? { nil }
}

extension WKWebView {
    func hideInputAccessoryView() {
        guard let target = findContentView(in: self) else { return }

        let baseClassName = String(describing: type(of: target))
        let noAccessoryClassName = "\(baseClassName)_NoInputAccessory"

        var patchedClass: AnyClass? = NSClassFromString(noAccessoryClassName)
        if patchedClass == nil {
            guard let targetClass = object_getClass(target),
                let nameCString = noAccessoryClassName.cString(using: .utf8),
                let allocatedClass = objc_allocateClassPair(targetClass, nameCString, 0)
            else {
                return
            }
            if let method = class_getInstanceMethod(
                NoInputAccessoryView.self,
                #selector(getter: NoInputAccessoryView.inputAccessoryView)
            ) {
                class_addMethod(
                    allocatedClass,
                    #selector(getter: UIResponder.inputAccessoryView),
                    method_getImplementation(method),
                    method_getTypeEncoding(method)
                )
            }
            objc_registerClassPair(allocatedClass)
            patchedClass = allocatedClass
        }

        if let patchedClass = patchedClass {
            object_setClass(target, patchedClass)
        }
    }

    private func findContentView(in view: UIView) -> UIView? {
        if String(describing: type(of: view)).hasPrefix("WKContent") {
            return view
        }
        for subview in view.subviews {
            if let found = findContentView(in: subview) {
                return found
            }
        }
        return nil
    }
}

class KeyboardFixPlugin: Plugin, WKScriptMessageHandler, ASWebAuthenticationPresentationContextProviding {
    private var keyboardObserver: NSObjectProtocol?
    private weak var patchedWebView: WKWebView?
    private var authSession: ASWebAuthenticationSession?

    @objc public override func load(webview: WKWebView) {
        patchedWebView = webview
        webview.configuration.userContentController.removeScriptMessageHandler(forName: "freeGrindFacebookOAuth")
        webview.configuration.userContentController.add(self, name: "freeGrindFacebookOAuth")

        keyboardObserver = NotificationCenter.default.addObserver(
            forName: UIResponder.keyboardWillShowNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.patchedWebView?.hideInputAccessoryView()
        }

        DispatchQueue.main.async { [weak self] in
            self?.patchedWebView?.hideInputAccessoryView()
        }
    }

    public func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        return patchedWebView?.window ?? UIWindow()
    }

    public func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        guard message.name == "freeGrindFacebookOAuth" else { return }
        DispatchQueue.main.async { [weak self] in
            self?.startFacebookOAuth()
        }
    }

    private func startFacebookOAuth() {
        authSession?.cancel()
        authSession = nil

        let oauthURLString =
            "https://m.facebook.com/v16.0/dialog/oauth?"
            + "client_id=1273378622718674"
            + "&redirect_uri=fb1273378622718674://authorize/"
            + "&response_type=token,signed_request,graph_domain"
            + "&scope=public_profile,email"
            + "&auth_type=rerequest"

        guard let url = URL(string: oauthURLString) else {
            dispatchFacebookOAuthResult(token: nil, error: "Invalid Facebook OAuth URL")
            return
        }

        let session = ASWebAuthenticationSession(
            url: url,
            callbackURLScheme: "fb1273378622718674"
        ) { [weak self] callbackURL, error in
            DispatchQueue.main.async {
                self?.authSession = nil
                if let callbackURL = callbackURL {
                    if let token = self?.extractParam(from: callbackURL, key: "access_token") {
                        self?.dispatchFacebookOAuthResult(token: token, error: nil)
                        return
                    }
                    if let err = self?.extractParam(from: callbackURL, key: "error_description")
                        ?? self?.extractParam(from: callbackURL, key: "error")
                    {
                        self?.dispatchFacebookOAuthResult(token: nil, error: err)
                        return
                    }
                    self?.dispatchFacebookOAuthResult(
                        token: nil,
                        error: "Missing access_token in Facebook callback"
                    )
                    return
                }

                let nsError = error as NSError?
                if nsError?.domain == ASWebAuthenticationSessionErrorDomain,
                   nsError?.code == ASWebAuthenticationSessionError.canceledLogin.rawValue
                {
                    self?.dispatchFacebookOAuthResult(token: nil, error: "Facebook login cancelled")
                } else {
                    self?.dispatchFacebookOAuthResult(
                        token: nil,
                        error: error?.localizedDescription ?? "Facebook login failed"
                    )
                }
            }
        }
        session.presentationContextProvider = self
        session.prefersEphemeralWebBrowserSession = false
        authSession = session

        if !session.start() {
            authSession = nil
            dispatchFacebookOAuthResult(token: nil, error: "Failed to start Facebook login session")
        }
    }

    private func extractParam(from url: URL, key: String) -> String? {
        if let fragment = url.fragment,
           let items = URLComponents(string: "https://dummy?\(fragment)")?.queryItems,
           let value = items.first(where: { $0.name == key })?.value,
           !value.isEmpty
        {
            return value
        }
        if let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems,
           let value = items.first(where: { $0.name == key })?.value,
           !value.isEmpty
        {
            return value
        }
        return nil
    }

    private func dispatchFacebookOAuthResult(token: String?, error: String?) {
        var payload: [String: String] = [:]
        if let token = token { payload["token"] = token }
        if let error = error { payload["error"] = error }
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let json = String(data: data, encoding: .utf8)
        else { return }
        let js = "window.dispatchEvent(new CustomEvent('fg:facebook-oauth', { detail: \(json) }));"
        patchedWebView?.evaluateJavaScript(js, completionHandler: nil)
    }

    deinit {
        if let keyboardObserver = keyboardObserver {
            NotificationCenter.default.removeObserver(keyboardObserver)
        }
        authSession?.cancel()
    }
}

@_cdecl("init_plugin_ios_keyboard_fix")
func initPlugin() -> Plugin {
    return KeyboardFixPlugin()
}
