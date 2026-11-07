package stirling.software.SPDF.model.api.security;

import static org.assertj.core.api.Assertions.assertThat;

import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.Test;
import org.springframework.mock.web.MockMultipartFile;

class SecurityRequestsTest {

    @Nested
    @DisplayName("SignPDFWithCertRequest")
    class SignCert {

        @Test
        @DisplayName("all accessors round-trip")
        void roundTrip() {
            SignPDFWithCertRequest req = new SignPDFWithCertRequest();
            req.setCertType("PKCS12");
            req.setPrivateKeyFile(new MockMultipartFile("k", new byte[] {1}));
            req.setCertFile(new MockMultipartFile("c", new byte[] {2}));
            req.setP12File(new MockMultipartFile("p", new byte[] {3}));
            req.setJksFile(new MockMultipartFile("j", new byte[] {4}));
            req.setPassword("pw");
            req.setShowSignature(true);
            req.setReason("because");
            req.setLocation("here");
            req.setName("Signer");
            req.setPageNumber(2);
            req.setShowLogo(false);

            assertThat(req.getCertType()).isEqualTo("PKCS12");
            assertThat(req.getPrivateKeyFile()).isNotNull();
            assertThat(req.getCertFile()).isNotNull();
            assertThat(req.getP12File()).isNotNull();
            assertThat(req.getJksFile()).isNotNull();
            assertThat(req.getPassword()).isEqualTo("pw");
            assertThat(req.getShowSignature()).isTrue();
            assertThat(req.getReason()).isEqualTo("because");
            assertThat(req.getLocation()).isEqualTo("here");
            assertThat(req.getName()).isEqualTo("Signer");
            assertThat(req.getPageNumber()).isEqualTo(2);
            assertThat(req.getShowLogo()).isFalse();
        }

        @Test
        @DisplayName("equals/hashCode/toString generated")
        void equality() {
            SignPDFWithCertRequest a = new SignPDFWithCertRequest();
            a.setCertType("JKS");
            SignPDFWithCertRequest b = new SignPDFWithCertRequest();
            b.setCertType("JKS");

            assertThat(a).isEqualTo(b).hasSameHashCodeAs(b);
            assertThat(a).isNotEqualTo(new SignPDFWithCertRequest());
            assertThat(a.toString()).contains("SignPDFWithCertRequest");
        }
    }

    @Nested
    @DisplayName("RedactPdfRequest")
    class Redact {

        @Test
        @DisplayName("text, flags, color and padding round-trip")
        void roundTrip() {
            RedactPdfRequest req = new RedactPdfRequest();
            req.setListOfText("a,b");
            req.setUseRegex(true);
            req.setWholeWordSearch(true);
            req.setRedactColor("#ff0000");
            req.setCustomPadding(2.5f);
            req.setConvertPDFToImage(true);

            assertThat(req.getListOfText()).isEqualTo("a,b");
            assertThat(req.getUseRegex()).isTrue();
            assertThat(req.getWholeWordSearch()).isTrue();
            assertThat(req.getRedactColor()).isEqualTo("#ff0000");
            assertThat(req.getCustomPadding()).isEqualTo(2.5f);
            assertThat(req.getConvertPDFToImage()).isTrue();
            assertThat(req.toString()).contains("RedactPdfRequest");
        }
    }
}
